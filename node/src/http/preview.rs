use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    extract::{Path, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use base64::Engine;
use parking_lot::Mutex;
use rand::RngCore;

use crate::{
    corpus::html::normalize_path,
    store::{now_ms, Store},
};

const IDLE_TTL: Duration = Duration::from_secs(60 * 60);
pub const PREVIEW_CSP: &str = "default-src 'none'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self' data:; media-src 'self' data: blob:; connect-src 'none'; worker-src 'none'; child-src 'none'; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'";

#[derive(Debug, Clone)]
pub struct PreviewBinding {
    pub channel: String,
    pub document_id: String,
    pub document_hash: String,
}

#[derive(Debug)]
struct TokenEntry {
    binding: PreviewBinding,
    last_used: Instant,
}

#[derive(Debug, Default)]
pub struct PreviewTokens {
    entries: Mutex<HashMap<String, TokenEntry>>,
}

impl PreviewTokens {
    pub fn mint(&self, binding: PreviewBinding) -> (String, i64) {
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let now = Instant::now();
        let mut entries = self.entries.lock();
        entries.retain(|_, entry| now.duration_since(entry.last_used) < IDLE_TTL);
        entries.insert(
            token.clone(),
            TokenEntry {
                binding,
                last_used: now,
            },
        );
        (token, now_ms() + IDLE_TTL.as_millis() as i64)
    }

    fn lookup(&self, token: &str) -> Option<PreviewBinding> {
        let now = Instant::now();
        let mut entries = self.entries.lock();
        if entries
            .get(token)
            .is_some_and(|entry| now.duration_since(entry.last_used) >= IDLE_TTL)
        {
            entries.remove(token);
            return None;
        }
        entries.get(token).map(|entry| entry.binding.clone())
    }

    fn refresh(&self, token: &str) {
        if let Some(entry) = self.entries.lock().get_mut(token) {
            entry.last_used = Instant::now();
        }
    }
}

#[derive(Clone)]
struct PreviewState {
    store: Arc<Store>,
    tokens: Arc<PreviewTokens>,
}

pub fn router(store: Arc<Store>, tokens: Arc<PreviewTokens>) -> Router {
    Router::new()
        .route("/p/{token}/{*path}", get(serve_resource))
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(middleware::from_fn(preview_headers))
        .with_state(PreviewState { store, tokens })
}

async fn serve_resource(
    State(state): State<PreviewState>,
    Path((token, path)): Path<(String, String)>,
) -> Response {
    let Some(binding) = state.tokens.lookup(&token) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(Some(document)) = state.store.doc_by_id(&binding.document_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if document.deleted != 0
        || document.format != "html"
        || document.channel != binding.channel
        || document.hash != binding.document_hash
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Ok(path) = normalize_path(&path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(files) = state.store.read_html_bundle(&document) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(file) = files.into_iter().find(|file| file.path == path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    state.tokens.refresh(&token);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, file.media_type)
        .body(axum::body::Body::from(file.bytes))
        .unwrap_or_else(|_| StatusCode::NOT_FOUND.into_response())
}

async fn preview_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, "private, no-store".parse().unwrap());
    headers.insert("referrer-policy", "no-referrer".parse().unwrap());
    headers.insert("x-content-type-options", "nosniff".parse().unwrap());
    headers.insert("content-security-policy", PREVIEW_CSP.parse().unwrap());
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_256_bit_url_safe_capabilities() {
        let tokens = PreviewTokens::default();
        let (token, expires_ms) = tokens.mint(PreviewBinding {
            channel: "personal".into(),
            document_id: "d".into(),
            document_hash: "h".into(),
        });
        assert_eq!(
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(&token)
                .unwrap()
                .len(),
            32
        );
        assert!(expires_ms > now_ms());
        assert_eq!(tokens.lookup(&token).unwrap().document_id, "d");
    }
}
