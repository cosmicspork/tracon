//! Building this node's vector index.
//!
//! The index is derived state: everything here can be thrown away and rebuilt
//! from the corpus, and nothing here is on the write path. A document is
//! written, committed and published first; the embedder notices afterwards.
//! That ordering is the point — an embedding endpoint that is slow, wrong, or
//! simply not running must cost search quality and nothing else.
//!
//! Every node embeds its own replica. Vectors do not replicate (see
//! `store::vectors`), so a node that cannot embed its own copy cannot search
//! it semantically, and the `processing` binding — which picks *one* node to
//! batch promotions — deliberately does not apply.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use crate::config::Config;
use crate::corpus::chunk;
use crate::store::vectors::Vector;
use crate::store::Store;
use crate::stream::{Bus, Frame};

pub struct Embedder {
    cfg: Arc<Config>,
    store: Arc<Store>,
    http: reqwest::Client,
}

#[derive(Deserialize)]
struct EmbeddingsResponse {
    data: Vec<EmbeddingDatum>,
}

#[derive(Deserialize)]
struct EmbeddingDatum {
    embedding: Vec<f32>,
    #[serde(default)]
    index: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("embedding endpoint: {0}")]
    Http(String),
    #[error("embedding endpoint returned {got} vectors for {want} inputs")]
    Count { want: usize, got: usize },
    #[error("embedding endpoint returned {got} dimensions, but [embed] dim is {want}")]
    Dim { want: usize, got: usize },
    #[error(transparent)]
    Store(#[from] rusqlite::Error),
}

impl Embedder {
    pub fn new(cfg: Arc<Config>, store: Arc<Store>, http: reqwest::Client) -> Self {
        Self { cfg, store, http }
    }

    /// Where the embeddings request goes. With `provider` set it goes through
    /// this node's own model gateway, so the egress allowlist and the daily
    /// ceiling still apply to it; otherwise straight at `base_url`, which is
    /// how a loopback model on this machine is reached without inventing a
    /// credential for it.
    fn endpoint(&self) -> String {
        let e = &self.cfg.embed;
        match &e.provider {
            Some(p) => format!(
                "http://127.0.0.1:{}/model/{p}/v1/embeddings",
                self.cfg.gateway.forward_port
            ),
            None => format!("{}/v1/embeddings", e.base_url.trim_end_matches('/')),
        }
    }

    /// Embed a batch of texts, in order. The order matters: the caller pairs
    /// the results back to its chunks by position, so a response that reorders
    /// or drops one is an error rather than a silent mispairing.
    pub async fn embed(&self, texts: &[String], token: &str) -> Result<Vec<Vec<f32>>, EmbedError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let e = &self.cfg.embed;
        let mut req = self
            .http
            .post(self.endpoint())
            .timeout(Duration::from_secs(e.timeout_secs))
            .json(&json!({ "model": e.model, "input": texts }));
        if e.provider.is_some() {
            req = req.bearer_auth(token);
        }
        let res = req
            .send()
            .await
            .map_err(|err| EmbedError::Http(err.to_string()))?;
        if !res.status().is_success() {
            let status = res.status();
            let body = res.text().await.unwrap_or_default();
            return Err(EmbedError::Http(format!(
                "{status}: {}",
                body.chars().take(200).collect::<String>()
            )));
        }
        let parsed: EmbeddingsResponse = res
            .json()
            .await
            .map_err(|err| EmbedError::Http(err.to_string()))?;
        if parsed.data.len() != texts.len() {
            return Err(EmbedError::Count {
                want: texts.len(),
                got: parsed.data.len(),
            });
        }
        // `index` is authoritative where the endpoint sets it; falling back to
        // position keeps a server that omits it working.
        let mut out = vec![Vec::new(); texts.len()];
        for (pos, d) in parsed.data.into_iter().enumerate() {
            let at = if d.index < out.len() { d.index } else { pos };
            if d.embedding.len() != e.dim {
                return Err(EmbedError::Dim {
                    want: e.dim,
                    got: d.embedding.len(),
                });
            }
            out[at] = d.embedding;
        }
        Ok(out)
    }

    /// Re-embed one record if what is indexed no longer matches its text.
    /// Returns how many chunks were written.
    pub async fn index_record(
        &self,
        table: &str,
        id: &str,
        channel: &str,
        body: &str,
        token: &str,
    ) -> Result<usize, EmbedError> {
        let chunks = match table {
            "document" => chunk::document(body),
            _ => chunk::memory(body),
        };
        // Compare what is indexed against what the body says now. Same count,
        // same hashes, same order means nothing to do — which is what keeps a
        // sweep over an unchanged corpus free.
        let have = self.store.vec_hashes(table, id)?;
        let want: Vec<String> = chunks.iter().map(|c| chunk::hash(&c.text)).collect();
        if !chunks.is_empty()
            && have.len() == want.len()
            && have.iter().zip(&want).all(|((_, h), w)| h == w)
        {
            return Ok(0);
        }
        // A body that lost its text, or a record that was deleted, leaves
        // nothing indexed rather than a vector pointing at text that is gone.
        self.store.vec_forget(table, id)?;
        if chunks.is_empty() {
            return Ok(0);
        }
        let batch_size = self.cfg.embed.batch.max(1);
        let mut written = 0;
        for (batch_ix, batch) in chunks.chunks(batch_size).enumerate() {
            let texts: Vec<String> = batch.iter().map(|c| c.text.clone()).collect();
            let vectors = self.embed(&texts, token).await?;
            for (within, (c, v)) in batch.iter().zip(vectors).enumerate() {
                self.store.vec_put(
                    &Vector {
                        source_table: table.into(),
                        source_id: id.into(),
                        chunk_ix: (batch_ix * batch_size + within) as i64,
                        channel: channel.into(),
                        model: self.cfg.embed.model.clone(),
                        text_hash: chunk::hash(&c.text),
                        offset: c.offset as i64,
                        len: c.len as i64,
                    },
                    &v,
                )?;
                written += 1;
            }
        }
        Ok(written)
    }

    /// Embed the query itself, for the search side.
    pub async fn embed_query(&self, text: &str, token: &str) -> Result<Vec<f32>, EmbedError> {
        let mut v = self.embed(&[text.to_string()], token).await?;
        Ok(v.pop().unwrap_or_default())
    }
}

/// The nearest indexed chunks to a query, or nothing at all when this node
/// does not embed, the endpoint is down, or the query embeds to nothing.
///
/// Nothing here returns an error. Semantic search is an addition to FTS, and a
/// node whose embedding endpoint is unreachable must answer the query with the
/// text results it already has rather than fail the search.
pub async fn neighbours(
    cfg: &Arc<Config>,
    store: &Arc<Store>,
    http: &reqwest::Client,
    token: &str,
    channel: Option<&str>,
    text: &str,
    limit: usize,
) -> Neighbours {
    if !cfg.embed.enabled || text.trim().is_empty() {
        return Neighbours::default();
    }
    let e = Embedder::new(cfg.clone(), store.clone(), http.clone());
    match e.embed_query(text, token).await {
        Ok(v) if !v.is_empty() => Neighbours {
            hits: store.vec_search(channel, &v, limit).unwrap_or_default(),
            degraded: false,
        },
        Ok(_) => Neighbours::default(),
        Err(err) => {
            // Configured but unreachable is worth saying out loud: the results
            // are narrower than this node normally gives, and a search that
            // quietly got worse is the kind of thing nobody notices.
            tracing::debug!(error = %err, "semantic search is unavailable; text only");
            Neighbours {
                hits: Vec::new(),
                degraded: true,
            }
        }
    }
}

/// What the vector leg found, and whether it was able to look at all.
#[derive(Debug, Default)]
pub struct Neighbours {
    pub hits: Vec<crate::store::vectors::Neighbour>,
    /// This node embeds, but the endpoint could not be reached for this query.
    pub degraded: bool,
}

/// Walk the corpus and embed whatever is missing or stale. Runs at startup and
/// again whenever replication brings something in.
async fn sweep(e: &Embedder, token: &str) {
    let docs = match e.store.rows_to_embed() {
        Ok(d) => d,
        Err(err) => {
            tracing::warn!(error = %err, "could not list documents to embed");
            return;
        }
    };
    let mut written = 0usize;
    let mut failed = 0usize;
    for (table, id, channel, body) in docs {
        match e.index_record(&table, &id, &channel, &body, token).await {
            Ok(n) => written += n,
            Err(err) => {
                failed += 1;
                // One bad record must not stop the sweep; the next pass tries
                // it again, and search stays FTS-only for it meanwhile.
                tracing::warn!(table, id, error = %err, "could not embed a record");
                if failed > 3 {
                    tracing::warn!("giving up this sweep; retrying later");
                    break;
                }
            }
        }
    }
    if written > 0 {
        tracing::info!(chunks = written, "embedded");
    }
}

/// The embedding task: a sweep at startup, then another whenever replicated
/// state changes. Returns immediately when `[embed]` is off, which is the
/// default and a complete configuration — retrieval is FTS5-only.
pub async fn run(
    cfg: Arc<Config>,
    store: Arc<Store>,
    bus: Bus,
    http: reqwest::Client,
    token: String,
) {
    if !cfg.embed.enabled {
        return;
    }
    if let Err(err) = store.vec_ensure(cfg.embed.dim) {
        tracing::warn!(error = %err, "could not open the vector index; retrieval stays text-only");
        return;
    }
    let e = Embedder::new(cfg, store, http);
    tracing::info!(model = %e.cfg.embed.model, dim = e.cfg.embed.dim, "embedding the corpus");
    sweep(&e, &token).await;

    let mut rx = bus.subscribe();
    loop {
        match rx.recv().await {
            Ok(Frame::Changes { .. }) => sweep(&e, &token).await,
            Ok(_) => {}
            // Lagged means changes were missed, and the sweep is a full
            // reconciliation anyway, so the recovery is simply to run it.
            Err(RecvError::Lagged(_)) => sweep(&e, &token).await,
            Err(RecvError::Closed) => return,
        }
    }
}
