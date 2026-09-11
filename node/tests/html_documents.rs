#[path = "support/mod.rs"]
mod support;

use std::io::{Cursor, Read};

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use tower::ServiceExt;

#[tokio::test]
async fn multipart_folder_import_round_trips_exact_files() {
    let h = support::harness::harness().await;
    let boundary = "tracon-html-boundary";
    let html = b"<!doctype html><title>Bundle demo</title><link rel=\"stylesheet\" href=\"assets/style.css\">";
    let css = b"body { color: rgb(1, 2, 3); }";
    let mut body = Vec::new();
    for (name, value) in [
        ("source_name", &b"demo-folder"[..]),
        ("entry_path", &b"index.html"[..]),
    ] {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n")
                .as_bytes(),
        );
        body.extend_from_slice(value);
        body.extend_from_slice(b"\r\n");
    }
    for (path, bytes, content_type) in [
        ("index.html", &html[..], "text/html"),
        ("assets/style.css", &css[..], "text/css"),
    ] {
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"files\"; filename=\"{path}\"\r\nContent-Type: {content_type}\r\n\r\n").as_bytes());
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

    let response = h
        .operator
        .clone()
        .oneshot(
            Request::post("/api/docs/personal/ref-bundle/html")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .header("if-none-match", "*")
                .body(Body::from(body.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(response["format"], "html");
    assert_eq!(response["entry_path"], "index.html");
    assert_eq!(response["source_name"], "demo-folder");
    assert_eq!(response["body"].as_str().unwrap().as_bytes(), html);

    let stale_replace = h
        .operator
        .clone()
        .oneshot(
            Request::post("/api/docs/personal/ref-bundle/html")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .header("if-match", "stale-generation")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stale_replace.status(), StatusCode::PRECONDITION_FAILED);

    let edit = h
        .operator
        .clone()
        .oneshot(
            Request::put("/api/docs/personal/ref-bundle")
                .header("content-type", "application/json")
                .header("if-match", response["hash"].as_str().unwrap())
                .body(Body::from("{\"body\":\"# overwritten\"}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(edit.status(), StatusCode::CONFLICT);
    let error: serde_json::Value =
        serde_json::from_slice(&edit.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(
        error["error"]["message"],
        "HTML documents must be replaced through import"
    );

    let archive = h
        .operator
        .clone()
        .oneshot(
            Request::put("/api/docs/personal/ref-bundle")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"archived":true}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(archive.status(), StatusCode::OK);
    let archived: serde_json::Value =
        serde_json::from_slice(&archive.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(archived["archived"], 1);
    assert_eq!(archived["body"].as_str().unwrap().as_bytes(), html);

    let download = h
        .operator
        .clone()
        .oneshot(
            Request::get("/api/docs/personal/ref-bundle/download")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(download.status(), StatusCode::OK);
    assert_eq!(download.headers()["content-type"], "application/zip");
    let zip_bytes = download.into_body().collect().await.unwrap().to_bytes();
    let mut archive = zip::ZipArchive::new(Cursor::new(zip_bytes)).unwrap();
    let mut downloaded_html = Vec::new();
    archive
        .by_name("index.html")
        .unwrap()
        .read_to_end(&mut downloaded_html)
        .unwrap();
    assert_eq!(downloaded_html, html);
    let mut downloaded_css = Vec::new();
    archive
        .by_name("assets/style.css")
        .unwrap()
        .read_to_end(&mut downloaded_css)
        .unwrap();
    assert_eq!(downloaded_css, css);

    let files: Vec<(String, i64)> = {
        let conn = h.store.conn();
        let files = conn
            .prepare(
                "SELECT path, size_bytes FROM document_bundle_file WHERE deleted = 0 ORDER BY path",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        files
    };
    assert_eq!(
        files,
        vec![
            ("assets/style.css".into(), css.len() as i64),
            ("index.html".into(), html.len() as i64)
        ]
    );
    let deleted = h
        .operator
        .clone()
        .oneshot(
            Request::delete("/api/docs/personal/ref-bundle")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(deleted.status(), StatusCode::OK);
    let download = h
        .operator
        .oneshot(
            Request::get("/api/docs/personal/ref-bundle/download")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(download.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn preview_capability_is_document_bound_and_replacement_invalidates_it() {
    use tracon::corpus::html::HtmlFile;

    let h = support::harness::harness().await;
    let (first, _) = h
        .store
        .write_html_document_change(
            "n1",
            "personal",
            "ref-first",
            "first.html",
            "first.html",
            vec![HtmlFile {
                path: "first.html".into(),
                bytes: b"<title>First</title><p>first generation</p>".to_vec(),
            }],
            None,
            true,
        )
        .unwrap();
    h.store
        .write_html_document_change(
            "n1",
            "personal",
            "ref-secret",
            "secret.html",
            "secret.html",
            vec![HtmlFile {
                path: "secret.html".into(),
                bytes: b"<title>Secret</title><p>not for the first token</p>".to_vec(),
            }],
            None,
            true,
        )
        .unwrap();

    let minted = h
        .operator
        .clone()
        .oneshot(
            Request::post("/api/docs/personal/ref-first/preview")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(minted.status(), StatusCode::OK);
    let minted: serde_json::Value =
        serde_json::from_slice(&minted.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let preview_url = url::Url::parse(minted["url"].as_str().unwrap()).unwrap();
    let segments: Vec<_> = preview_url.path_segments().unwrap().collect();
    let token = segments[1];
    let preview = tracon::http::preview::router(h.store.clone(), h.manager.previews().clone());

    let served = preview
        .clone()
        .oneshot(
            Request::get(preview_url.path())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(served.status(), StatusCode::OK);
    assert_eq!(served.headers()["cache-control"], "private, no-store");
    assert_eq!(
        served.headers()["content-security-policy"],
        tracon::http::preview::PREVIEW_CSP
    );
    assert_eq!(served.headers()["referrer-policy"], "no-referrer");
    assert_eq!(served.headers()["x-content-type-options"], "nosniff");
    assert!(!served.headers().contains_key("access-control-allow-origin"));
    assert_eq!(
        served.into_body().collect().await.unwrap().to_bytes(),
        &b"<title>First</title><p>first generation</p>"[..]
    );

    for forbidden in ["/api/node".to_string(), format!("/p/{token}/secret.html")] {
        let response = preview
            .clone()
            .oneshot(Request::get(forbidden).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    h.store
        .write_html_document_change(
            "n1",
            "personal",
            "ref-first",
            "first.html",
            "first.html",
            vec![HtmlFile {
                path: "first.html".into(),
                bytes: b"<title>First</title><p>second generation</p>".to_vec(),
            }],
            Some(&first.hash),
            false,
        )
        .unwrap();
    let stale = preview
        .oneshot(
            Request::get(preview_url.path())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::NOT_FOUND);
}
