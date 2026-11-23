//! A repository's image built once and taken by the channel co-members that
//! accepted the same recipe.
//!
//! The recipe is shared between nodes (`crate::mesh::recipe`) and the image
//! is not: each node pins what it built, under a digest of its own. A Podman
//! node with DigitalOcean's container registry (`[digitalocean] registry`
//! and `credential`) also pushes each repository image it builds there and
//! tells its channels so, in a statement signed with its node key
//! ([`proto::image::ImageStatement`]): this forge identity, this recipe
//! hash, this platform, this digest. A co-member about to build the same
//! recipe for the same platform pulls that digest instead, with its own
//! DigitalOcean token, and pins it as its own build. Nothing else it is
//! handed is taken: the statement has to verify under its signer's key, the
//! signer has to share the channel it came on with this node still, and
//! what the registry serves has to be the digest signed. A node without
//! access to the registry, or one the image fails any of that for, builds
//! the recipe itself, and its build log says why.
//!
//! The recipe hash is the one each node keys its build on
//! (`crate::repo_image::Recipe::hash`), which is computed from portable
//! inputs alone: the build steps and their hosts, or the Dockerfile and its
//! context as Git objects, and the stock base pinned by its platform digest.
//! Two nodes that agree on it would build the same thing.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use proto::frame::{Payload, MESH_CHANNEL};
use proto::image::ImageStatement;

use crate::boundary::registry::{Reference, Registry, DIGITALOCEAN_HOST};
use crate::boundary::{Backend, ImageBuilder};
use crate::config::{Config, RuntimeKind};
use crate::store::Store;

/// The repository in `[digitalocean] registry` every repository image is
/// pushed to, under the node's resource prefix: one repository, a tag per
/// recipe and platform, since DigitalOcean's plans count repositories.
pub const REPOSITORY: &str = "repo-images";

/// What sharing needs of the running node.
struct Context {
    cfg: Arc<Config>,
    store: Arc<Store>,
    backend: Arc<dyn Backend>,
    broker: crate::broker::SharedBroker,
    mesh: Arc<crate::mesh::client::MeshClient>,
}

static CONTEXT: OnceLock<Context> = OnceLock::new();

/// Share and take repository images from here on: called once the node is
/// on a mesh. A node on no mesh has no co-member to share with.
pub fn install(
    cfg: Arc<Config>,
    store: Arc<Store>,
    backend: Arc<dyn Backend>,
    broker: crate::broker::SharedBroker,
    mesh: Arc<crate::mesh::client::MeshClient>,
) {
    let _ = CONTEXT.set(Context {
        cfg,
        store,
        backend,
        broker,
        mesh,
    });
}

/// The platform this node builds for and takes images for.
pub fn platform() -> String {
    format!("linux/{}", crate::boundary::registry::host_architecture())
}

/// What a pull or a push presents to DigitalOcean's registry: the API
/// token, which its registry takes as both user and password, as a Docker
/// auth file for Podman and as a credential for the registry client.
pub(crate) struct Docr {
    host: String,
    registry: Registry,
    dir: tempfile::TempDir,
}

impl Docr {
    pub(crate) fn new(host: &str, token: &str, registry: Registry) -> Result<Self, String> {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
        let auth = dir.path().join("auth.json");
        std::fs::write(
            &auth,
            crate::provider::digitalocean::images::auth_json(host, token),
        )
        .map_err(|e| e.to_string())?;
        std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
        Ok(Docr {
            host: host.to_string(),
            registry: registry.with_credential(host, token, token),
            dir,
        })
    }

    /// A test's registry, presenting the credential it was made with.
    #[cfg(test)]
    fn for_test(host: &str, registry: Registry) -> Result<Self, String> {
        let mut docr = Docr::new(host, "unused", Registry::new(None))?;
        docr.registry = registry;
        Ok(docr)
    }

    fn auth_file(&self) -> PathBuf {
        self.dir.path().join("auth.json")
    }
}

/// This node's access to DigitalOcean's registry: the token its
/// `[digitalocean] credential` holds.
fn docr(cfg: &Config, broker: &crate::broker::SharedBroker, node_id: &str) -> Result<Docr, String> {
    let name = cfg.digitalocean.credential.trim();
    if name.is_empty() {
        return Err("no [digitalocean] credential names a DigitalOcean token".into());
    }
    let env = broker
        .read()
        .map_err(|_| "the credential store is unavailable".to_string())?
        .env_for_node(name, node_id)
        .map_err(|e| format!("DigitalOcean credential {name}: {e}"))?;
    let token = env
        .get(crate::do_harness::TOKEN_ENV)
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| {
            format!(
                "DigitalOcean credential {name} has no {}",
                crate::do_harness::TOKEN_ENV
            )
        })?;
    Docr::new(DIGITALOCEAN_HOST, token.trim(), Registry::new(None))
}

/// Where this node pushes its repository images, when it does:
/// `<[digitalocean] registry>/<prefix>-repo-images`. `None` on a runtime
/// that builds into a registry of its own, or with no DigitalOcean registry.
pub fn destination(cfg: &Config) -> Option<String> {
    if cfg.runtime.kind != RuntimeKind::Podman || !cfg.digitalocean.is_configured() {
        return None;
    }
    let registry = cfg.digitalocean.registry.trim().trim_end_matches('/');
    let reference = Reference::parse(&format!("{registry}/x")).ok()?;
    (reference.host == DIGITALOCEAN_HOST)
        .then(|| format!("{registry}/{}", crate::config::resource_name(REPOSITORY)))
}

/// The tag one recipe's image is pushed under, for one platform.
pub fn tag(repository: &str, recipe_hash: &str, platform: &str) -> String {
    let arch = platform.rsplit('/').next().unwrap_or(platform);
    format!(
        "{repository}:{}-{arch}",
        &recipe_hash[..recipe_hash.len().min(32)]
    )
}

// ------------------------------------------------------------------ sharing

/// Push the image this node just built for `repo` to its DigitalOcean
/// registry and tell its channels, in the background: a session waiting on
/// the build does not wait on the push. Nothing happens on a node that does
/// not share (no mesh, not Podman, no DigitalOcean registry), or for a
/// repository with no forge identity, which no other node could name.
pub fn after_build(repo: &Path, recipe_hash: &str, image: &str) {
    let Some(ctx) = CONTEXT.get() else {
        return;
    };
    let Some(destination) = destination(&ctx.cfg) else {
        return;
    };
    let (repo, recipe_hash, image) = (
        repo.to_path_buf(),
        recipe_hash.to_string(),
        image.to_string(),
    );
    tokio::spawn(async move {
        let Some(identity) =
            crate::provider::digitalocean::portable::forge_identity(&ctx.cfg, &repo).await
        else {
            tracing::debug!(repo = %repo.display(), "repository image not shared: the repository has no forge identity");
            return;
        };
        let Some(builder) = ctx.backend.image_builder() else {
            return;
        };
        let node_id = ctx.mesh.node_id();
        let shared = match docr(&ctx.cfg, &ctx.broker, &node_id) {
            Ok(docr) => {
                publish(
                    &ctx.store,
                    ctx.mesh.identity(),
                    builder,
                    &docr,
                    &destination,
                    &identity,
                    &recipe_hash,
                    &image,
                )
                .await
            }
            Err(error) => Err(error),
        };
        match shared {
            Ok(statement) => {
                tracing::info!(repo = %identity, image = %statement.image, "repository image pushed for the channel's other members");
                send(&ctx.store, &ctx.mesh, &[statement]);
            }
            Err(error) => {
                tracing::warn!(repo = %identity, %error, "repository image not shared with the channel's other members")
            }
        }
    });
}

/// Push `image` to `destination` and sign what the registry holds it as.
/// The statement is kept as this node's own, which every snapshot repeats.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn publish(
    store: &Store,
    signer: &proto::keys::Identity,
    builder: &dyn ImageBuilder,
    docr: &Docr,
    destination: &str,
    repo: &str,
    recipe_hash: &str,
    image: &str,
) -> Result<ImageStatement, String> {
    let platform = platform();
    let tagged = tag(destination, recipe_hash, &platform);
    let digest = builder.push_to(image, &tagged, &docr.auth_file()).await?;
    let pushed = format!("{destination}@{digest}");
    let statement = ImageStatement::sign(
        signer,
        repo,
        recipe_hash,
        &platform,
        &pushed,
        crate::store::now_ms(),
    );
    statement
        .verify()
        .map_err(|e| format!("the registry answered the push with {digest:?}: {e}"))?;
    store
        .repo_image_statement_put("", &statement, crate::store::now_ms())
        .map_err(|e| e.to_string())?;
    Ok(statement)
}

/// Send `statements` to every channel this node holds a key for, `@mesh`
/// aside: an image is for the nodes this one works with.
fn send(store: &Store, mesh: &crate::mesh::client::MeshClient, statements: &[ImageStatement]) {
    for (channel, payload) in payloads(store, &mesh.node_id(), statements) {
        if let Err(error) = mesh.enqueue(&channel, None, &payload) {
            tracing::debug!(%channel, %error, "repository images not sent");
        }
    }
}

/// One payload per channel `self_id` holds a key for, `@mesh` aside.
pub fn payloads(
    store: &Store,
    self_id: &str,
    statements: &[ImageStatement],
) -> Vec<(String, Payload)> {
    if statements.is_empty() {
        return Vec::new();
    }
    store
        .node_channels_keyed(self_id)
        .unwrap_or_default()
        .into_iter()
        .filter(|channel| channel != MESH_CHANNEL)
        .map(|channel| {
            (
                channel,
                Payload::RepoImages {
                    images: statements.to_vec(),
                },
            )
        })
        .collect()
}

/// This node's own statements, for a snapshot.
pub fn own(store: &Store, self_id: &str) -> Vec<ImageStatement> {
    store
        .repo_image_statements_own(self_id)
        .unwrap_or_default()
        .into_iter()
        .map(|row| row.statement)
        .collect()
}

/// What receiving statements came to.
#[derive(Debug, PartialEq, Eq)]
pub enum Received {
    Stored,
    /// A statement another node signed: a node speaks only for itself.
    Impersonation,
    Malformed(String),
}

/// Keep what `sender` says it built, received on `channel`. Each statement
/// must be its own and verify; one that does not drops the payload.
pub fn receive(store: &Store, sender: &str, channel: &str, images: &[ImageStatement]) -> Received {
    if channel == MESH_CHANNEL {
        return Received::Malformed("repository images are sent on a member channel".into());
    }
    if images.iter().any(|statement| statement.signer != sender) {
        return Received::Impersonation;
    }
    if let Some(error) = images.iter().find_map(|s| s.verify().err()) {
        return Received::Malformed(error.to_string());
    }
    let now = crate::store::now_ms();
    for statement in images {
        if let Err(error) = store.repo_image_statement_put(channel, statement, now) {
            return Received::Malformed(error.to_string());
        }
    }
    Received::Stored
}

// ------------------------------------------------------------------- taking

/// What looking for a co-member's image came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Taken {
    /// Pulled and checked: `image` is it here, by digest, and `note` says
    /// whose it was.
    Image { image: String, note: String },
    /// No co-member said it built this recipe.
    NotOffered,
    /// One did, and it could not be taken, for this reason.
    Refused(String),
}

impl Taken {
    /// The line a build made in its place opens its log with.
    pub fn refusal(&self) -> Option<String> {
        match self {
            Taken::Refused(why) => Some(format!(
                "Built on this node rather than taken from a channel member's build: {why}"
            )),
            _ => None,
        }
    }
}

/// Take a co-member's image of `recipe_hash` for `repo` in place of a
/// build, tagged `tag` here, where this node shares at all.
pub async fn take(
    cfg: &Config,
    store: &Store,
    builder: &dyn ImageBuilder,
    repo: &Path,
    recipe_hash: &str,
    tag: &str,
) -> Taken {
    let Some(ctx) = CONTEXT.get() else {
        return Taken::NotOffered;
    };
    if cfg.runtime.kind != RuntimeKind::Podman {
        return Taken::NotOffered;
    }
    let Some(identity) = crate::provider::digitalocean::portable::forge_identity(cfg, repo).await
    else {
        return Taken::NotOffered;
    };
    let node_id = ctx.mesh.node_id();
    take_with(
        store,
        &node_id,
        builder,
        &identity,
        recipe_hash,
        &platform(),
        tag,
        || docr(cfg, &ctx.broker, &node_id),
    )
    .await
}

/// `take`, with what it needs given.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn take_with(
    store: &Store,
    self_id: &str,
    builder: &dyn ImageBuilder,
    repo: &str,
    recipe_hash: &str,
    platform: &str,
    tag: &str,
    access: impl FnOnce() -> Result<Docr, String>,
) -> Taken {
    let rows = store
        .repo_image_statements_for(self_id, repo, recipe_hash, platform)
        .unwrap_or_default();
    if rows.is_empty() {
        return Taken::NotOffered;
    }
    let mine = store.node_channels(self_id).unwrap_or_default();
    let name = |id: &str| {
        store
            .get_node(id)
            .ok()
            .flatten()
            .map(|row| row.name)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| id[..id.len().min(12)].to_string())
    };
    let mut refused = Vec::new();
    let mut usable = Vec::new();
    for row in rows {
        let statement = &row.statement;
        let who = name(&statement.signer);
        if let Err(error) = statement.verify() {
            refused.push(format!("{who}'s statement: {error}"));
        } else if !mine.contains(&row.channel)
            || !store
                .nodes_in_channel(&row.channel)
                .unwrap_or_default()
                .contains(&statement.signer)
        {
            refused.push(format!(
                "{who} no longer shares {} with this node",
                row.channel
            ));
        } else {
            usable.push((who, row));
        }
    }
    if usable.is_empty() {
        return Taken::Refused(refused.join("; "));
    }
    let docr = match access() {
        Ok(docr) => docr,
        Err(error) => {
            return Taken::Refused(format!(
                "this node has no access to DigitalOcean's container registry: {error}"
            ))
        }
    };
    for (who, row) in usable {
        let statement = &row.statement;
        let signed = statement.digest();
        match Reference::parse(&statement.image) {
            Ok(reference) if reference.host == docr.host => {}
            _ => {
                refused.push(format!(
                    "{who}'s image {} is not in DigitalOcean's container registry",
                    statement.image
                ));
                continue;
            }
        }
        match docr.registry.digest(&statement.image).await {
            Ok(Some(digest)) if digest == signed => {}
            Ok(Some(digest)) => {
                refused.push(format!(
                    "the registry serves {who}'s image as {digest}, not the {signed} it signed"
                ));
                continue;
            }
            Ok(None) => {
                refused.push(format!(
                    "{who}'s image {} is no longer in the registry",
                    statement.image
                ));
                continue;
            }
            Err(error) => {
                refused.push(format!(
                    "this node could not read {who}'s image from DigitalOcean's container \
                     registry: {error}"
                ));
                continue;
            }
        }
        let pulled = match builder
            .pull_as(&statement.image, &docr.auth_file(), tag)
            .await
        {
            Ok(pulled) => pulled,
            Err(error) => {
                refused.push(format!("{who}'s image: {error}"));
                continue;
            }
        };
        if pulled.rsplit_once('@').map(|(_, digest)| digest) != Some(signed) {
            builder.remove(&pulled).await;
            refused.push(format!(
                "{who}'s image pulled as {pulled}, not the {signed} it signed"
            ));
            continue;
        }
        let mut note = format!(
            "Taken from {who}'s build of the same recipe, signed for {}: {}",
            row.channel, statement.image
        );
        if !refused.is_empty() {
            note.push_str(&format!(" (passed over: {})", refused.join("; ")));
        }
        return Taken::Image {
            image: pulled,
            note,
        };
    }
    Taken::Refused(refused.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boundary::registry::tests::{serve, Fake};
    use crate::boundary::{BuildFailure, BuiltImage, ImageBuild};
    use proto::keys::Identity;
    use std::sync::Mutex;

    const RECIPE: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const REPO: &str = "github.com/acme/app";

    /// A local store that pulls and pushes as `podman` would, as told.
    #[derive(Default)]
    struct LocalStore {
        pulled: Mutex<Vec<String>>,
        removed: Mutex<Vec<String>>,
        /// What a pull leaves the tag naming, instead of the digest asked.
        serves: Mutex<Option<String>>,
        /// What a push answers.
        pushed: Mutex<Option<String>>,
    }

    #[async_trait::async_trait]
    impl ImageBuilder for LocalStore {
        async fn build(&self, _: ImageBuild<'_>) -> Result<BuiltImage, BuildFailure> {
            unreachable!()
        }
        async fn exists(&self, _: &str) -> bool {
            true
        }
        async fn identity(&self, image: &str) -> Option<String> {
            Some(image.into())
        }
        async fn remove(&self, image: &str) {
            self.removed.lock().unwrap().push(image.into());
        }
        async fn push_to(&self, _: &str, _: &str, auth: &Path) -> Result<String, String> {
            assert!(auth.is_file());
            self.pushed.lock().unwrap().clone().ok_or("refused".into())
        }
        async fn pull_as(&self, image: &str, auth: &Path, tag: &str) -> Result<String, String> {
            assert!(auth.is_file());
            self.pulled.lock().unwrap().push(image.into());
            let digest = self
                .serves
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| image.rsplit_once('@').unwrap().1.to_string());
            let name = tag.rsplit_once(':').unwrap().0;
            Ok(format!("{name}@{digest}"))
        }
    }

    fn digest(n: char) -> String {
        format!("sha256:{}", n.to_string().repeat(64))
    }

    struct World {
        store: Store,
        me: Identity,
        peer: Identity,
        fake: Arc<Fake>,
        host: String,
    }

    async fn world() -> World {
        let store = Store::open_in_memory().unwrap();
        let me = Identity::from_seed(&[1; 32]);
        let peer = Identity::from_seed(&[2; 32]);
        store
            .node_channels_set(&me.node_id(), &["team".into(), "@mesh".into()])
            .unwrap();
        store
            .node_channels_set(&peer.node_id(), &["team".into(), "@mesh".into()])
            .unwrap();
        let (fake, host) = serve().await;
        World {
            store,
            me,
            peer,
            fake,
            host,
        }
    }

    impl World {
        /// Access as `pusher` with `password`; the fake's token service
        /// takes `secret`.
        fn docr(&self, password: &str) -> Result<Docr, String> {
            Docr::for_test(
                &self.host,
                Registry::new(None)
                    .with_credential(&self.host, "pusher", password)
                    .for_test("unused", "unused"),
            )
        }

        fn signed(&self, by: &Identity, image_digest: &str) -> ImageStatement {
            ImageStatement::sign(
                by,
                REPO,
                RECIPE,
                "linux/amd64",
                &format!("{}/acme/tracon-repo-images@{image_digest}", self.host),
                crate::store::now_ms(),
            )
        }

        async fn take(&self, builder: &LocalStore, token: &str) -> Taken {
            take_with(
                &self.store,
                &self.me.node_id(),
                builder,
                REPO,
                RECIPE,
                "linux/amd64",
                "localhost/tracon-repo-app-0123456789:0123456789ab-1",
                || self.docr(token),
            )
            .await
        }
    }

    /// A co-member's signed image of the same recipe is pulled by the
    /// digest it signed and pinned under this node's own tag; with nothing
    /// said for the recipe, the node builds without a word.
    #[tokio::test]
    async fn a_co_members_signed_image_is_taken_in_place_of_a_build() {
        let w = world().await;
        let builder = LocalStore::default();
        assert_eq!(w.take(&builder, "secret").await, Taken::NotOffered);

        let d = digest('1');
        w.fake.put("acme/tracon-repo-images", "x", &d);
        let statement = w.signed(&w.peer, &d);
        assert_eq!(
            receive(
                &w.store,
                &w.peer.node_id(),
                "team",
                std::slice::from_ref(&statement)
            ),
            Received::Stored
        );
        match w.take(&builder, "wrong").await {
            Taken::Refused(why) => assert!(why.contains("could not read"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert!(builder.pulled.lock().unwrap().is_empty());
    }

    /// The registry's answer, what the pull leaves, the signature and the
    /// signer's membership all have to agree with the statement, or the node
    /// builds and says why.
    #[tokio::test]
    async fn an_image_that_is_not_what_was_signed_is_refused_and_built() {
        let w = world().await;
        let d = digest('1');
        let statement = w.signed(&w.peer, &d);
        receive(&w.store, &w.peer.node_id(), "team", &[statement]);

        // Not in the registry (any more).
        let builder = LocalStore::default();
        match w.take(&builder, "secret").await {
            Taken::Refused(why) => assert!(why.contains("no longer in the registry"), "{why}"),
            other => panic!("{other:?}"),
        }

        // In it, but the pull leaves another image under the tag.
        w.fake.put("acme/tracon-repo-images", "x", &d);
        *builder.serves.lock().unwrap() = Some(digest('2'));
        match w.take(&builder, "secret").await {
            Taken::Refused(why) => assert!(why.contains("not the sha256:111"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(builder.removed.lock().unwrap().len(), 1);

        let refused = take_with(
            &w.store,
            &w.me.node_id(),
            &builder,
            REPO,
            RECIPE,
            "linux/amd64",
            "localhost/x:1",
            || Err("no [digitalocean] credential names a DigitalOcean token".into()),
        )
        .await;
        assert_eq!(
            refused.refusal().unwrap(),
            "Built on this node rather than taken from a channel member's build: this node \
             has no access to DigitalOcean's container registry: no [digitalocean] credential \
             names a DigitalOcean token"
        );

        // The signer left the channel.
        w.store
            .node_channels_set(&w.peer.node_id(), &["@mesh".into()])
            .unwrap();
        *builder.serves.lock().unwrap() = None;
        match w.take(&builder, "secret").await {
            Taken::Refused(why) => assert!(why.contains("no longer shares team"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_verified_image_is_pinned_by_the_digest_signed() {
        let w = world().await;
        let d = digest('1');
        w.fake.put("acme/tracon-repo-images", "x", &d);
        receive(
            &w.store,
            &w.peer.node_id(),
            "team",
            &[w.signed(&w.peer, &d)],
        );
        let builder = LocalStore::default();
        match w.take(&builder, "secret").await {
            Taken::Image { image, note } => {
                assert_eq!(image, format!("localhost/tracon-repo-app-0123456789@{d}"));
                assert!(note.contains("signed for team"), "{note}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            *builder.pulled.lock().unwrap(),
            vec![format!("{}/acme/tracon-repo-images@{d}", w.host)]
        );
    }

    /// A node speaks only for itself, on a member channel, in statements
    /// that verify.
    #[tokio::test]
    async fn only_a_senders_own_verified_statements_are_kept() {
        let w = world().await;
        let theirs = w.signed(&w.peer, &digest('1'));
        assert_eq!(
            receive(
                &w.store,
                &w.me.node_id(),
                "team",
                std::slice::from_ref(&theirs)
            ),
            Received::Impersonation
        );
        assert!(matches!(
            receive(
                &w.store,
                &w.peer.node_id(),
                MESH_CHANNEL,
                std::slice::from_ref(&theirs)
            ),
            Received::Malformed(_)
        ));
        let mut forged = theirs.clone();
        forged.image = forged.image.replace("1111", "2222");
        assert!(matches!(
            receive(&w.store, &w.peer.node_id(), "team", &[forged]),
            Received::Malformed(_)
        ));
        assert!(w
            .store
            .repo_image_statements_for(&w.me.node_id(), REPO, RECIPE, "linux/amd64")
            .unwrap()
            .is_empty());
    }

    /// A build is pushed under its recipe's tag and signed as the digest the
    /// registry took; the statement is this node's own and goes to its
    /// member channels, never `@mesh`.
    #[tokio::test]
    async fn a_build_is_pushed_signed_and_sent_to_member_channels() {
        let w = world().await;
        let builder = LocalStore::default();
        let docr = w.docr("secret").unwrap();
        let destination = "registry.digitalocean.com/acme/tracon-repo-images";
        let error = publish(
            &w.store,
            &w.me,
            &builder,
            &docr,
            destination,
            REPO,
            RECIPE,
            "localhost/tracon-repo-app@sha256:aa",
        )
        .await
        .unwrap_err();
        assert_eq!(error, "refused");

        *builder.pushed.lock().unwrap() = Some(digest('3'));
        let statement = publish(
            &w.store,
            &w.me,
            &builder,
            &docr,
            destination,
            REPO,
            RECIPE,
            "localhost/tracon-repo-app@sha256:aa",
        )
        .await
        .unwrap();
        assert_eq!(statement.image, format!("{destination}@{}", digest('3')));
        assert_eq!(statement.signer, w.me.node_id());
        statement.verify().unwrap();
        assert_eq!(own(&w.store, &w.me.node_id()), vec![statement.clone()]);

        let sent = payloads(&w.store, &w.me.node_id(), std::slice::from_ref(&statement));
        // Only channels this node holds a key for, and never `@mesh`.
        assert!(sent.is_empty());
        w.store.channel_put("team", b"ring", "{}").unwrap();
        w.store.channel_put(MESH_CHANNEL, b"ring", "{}").unwrap();
        let sent = payloads(&w.store, &w.me.node_id(), std::slice::from_ref(&statement));
        assert_eq!(
            sent,
            vec![(
                "team".to_string(),
                Payload::RepoImages {
                    images: vec![statement]
                }
            )]
        );
        assert_eq!(
            tag(destination, RECIPE, "linux/amd64"),
            format!("{destination}:0123456789abcdef0123456789abcdef-amd64")
        );
    }
}
