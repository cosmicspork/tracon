use axum::{
    http::{header, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use rust_embed::Embed;

use super::api::ApiError;

#[derive(Embed)]
#[folder = "../spa/dist"]
struct Assets;

/// The build of the interface this binary embeds, as the SPA's build wrote it
/// into `build-id`: empty for a release bundle, `None` when the bundle carries
/// no id (a placeholder, or one built before ids existed). A page compares its
/// own against this to know whether a reload would change it.
pub fn interface_build() -> Option<String> {
    Assets::get("build-id").map(|file| String::from_utf8_lossy(&file.data).trim().to_string())
}

/// The prefixes only machines ask for: the operator API and the tools door
/// (`/mcp/external/{channel}`). No interface route lives under them, so a
/// path there that no route matched is a client asking for something this
/// node does not have, not a deep link for the interface's router.
const API_PREFIXES: &[&str] = &["/api", "/mcp"];

/// Whether `path` is `/api`, `/mcp`, or under one of them.
pub(crate) fn api_only(path: &str) -> bool {
    API_PREFIXES.iter().any(|prefix| {
        path.strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    })
}

/// What no route matched. Under an API prefix that is a 404 in the API's own
/// error shape, so a client expecting JSON reads "no such route" rather than
/// failing to parse the interface's HTML; anything else is the interface.
pub async fn fallback(method: Method, uri: Uri) -> Response {
    if api_only(uri.path()) {
        return ApiError::new(
            StatusCode::NOT_FOUND,
            format!("no API route {method} {}", uri.path()),
        )
        .into_response();
    }
    serve(uri).await
}

/// Serves the embedded SPA. Unknown paths fall back to `index.html` so the
/// client-side router owns every route the API does not.
pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    let (path, file) = match Assets::get(path) {
        Some(file) => (path, Some(file)),
        None if path.starts_with("assets/") => (path, None),
        None => ("index.html", Assets::get("index.html")),
    };
    match file {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            let cache = if path.starts_with("assets/") {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            };
            (
                [
                    (header::CONTENT_TYPE, mime.as_ref().to_string()),
                    (header::CACHE_CONTROL, cache.to_string()),
                ],
                file.data,
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::api_only;

    #[test]
    fn only_paths_at_or_under_an_api_prefix_are_api_only() {
        for path in ["/api", "/api/", "/api/nope", "/mcp", "/mcp/", "/mcp/nope/x"] {
            assert!(api_only(path), "{path}");
        }
        for path in [
            "/",
            "/apis",
            "/api-docs",
            "/mcpx",
            "/settings",
            "/docs/personal/some-slug",
            "/plugins/hoja/ui",
        ] {
            assert!(!api_only(path), "{path}");
        }
    }
}
