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

    // A Markdown body over it is a conversion, held to the same precondition.
    let stale_edit = h
        .operator
        .clone()
        .oneshot(
            Request::put("/api/docs/personal/ref-bundle")
                .header("content-type", "application/json")
                .header("if-match", "stale-generation")
                .body(Body::from("{\"body\":\"# overwritten\"}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stale_edit.status(), StatusCode::PRECONDITION_FAILED);

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
            &[],
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
            None,
            None,
        )
        .unwrap();
    h.store
        .write_html_document_change(
            &[],
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
            None,
            None,
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
            &[],
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
            None,
            None,
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

async fn json_of(response: axum::response::Response) -> serde_json::Value {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

/// `PUT` with a body converts an HTML document to Markdown in place, and
/// `format: "html"` converts it back as a single-file bundle. Once it is
/// Markdown there is nothing to download as a bundle and no page to preview.
#[tokio::test]
async fn a_put_body_converts_an_html_document_to_markdown_and_back() {
    use tracon::corpus::html::HtmlFile;

    let h = support::harness::harness().await;
    let (html, _) = h
        .store
        .write_html_document_change(
            &[],
            "n1",
            "personal",
            "ref-flip",
            "flip",
            "index.html",
            vec![
                HtmlFile {
                    path: "index.html".into(),
                    bytes: b"<title>Flip</title><link rel=stylesheet href=a.css>".to_vec(),
                },
                HtmlFile {
                    path: "a.css".into(),
                    bytes: b"p {}".to_vec(),
                },
            ],
            None,
            true,
            None,
            Some(true),
        )
        .unwrap();
    let minted = h
        .operator
        .clone()
        .oneshot(
            Request::post("/api/docs/personal/ref-flip/preview")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(minted.status(), StatusCode::OK);
    let preview_url = url::Url::parse(json_of(minted).await["url"].as_str().unwrap()).unwrap();

    let put = |body: &'static str, if_match: String| {
        Request::put("/api/docs/personal/ref-flip")
            .header("content-type", "application/json")
            .header("if-match", if_match)
            .body(Body::from(body))
            .unwrap()
    };
    let converted = h
        .operator
        .clone()
        .oneshot(put(
            r##"{"body":"# Flip\n\nNow Markdown."}"##,
            html.hash.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(converted.status(), StatusCode::OK);
    let converted = json_of(converted).await;
    assert_eq!(converted["format"], "markdown");
    assert_eq!(converted["id"], html.id.as_str());
    assert_eq!(converted["pinned"], 1);
    assert_eq!(converted["entry_path"], serde_json::Value::Null);

    let download = h
        .operator
        .clone()
        .oneshot(
            Request::get("/api/docs/personal/ref-flip/download")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(download.status(), StatusCode::CONFLICT);
    let preview = tracon::http::preview::router(h.store.clone(), h.manager.previews().clone());
    let stale = preview
        .oneshot(
            Request::get(preview_url.path())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::NOT_FOUND);

    let back = h
        .operator
        .clone()
        .oneshot(put(
            r#"{"body":"<title>Back</title>","format":"html"}"#,
            converted["hash"].as_str().unwrap().to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(back.status(), StatusCode::OK);
    let back = json_of(back).await;
    assert_eq!(back["format"], "html");
    assert_eq!(back["id"], html.id.as_str());
    assert_eq!(back["entry_path"], "ref-flip.html");
    let doc = h.store.doc_get("personal", "ref-flip").unwrap().unwrap();
    assert_eq!(h.store.read_html_bundle(&doc).unwrap().len(), 1);

    let unknown = h
        .operator
        .clone()
        .oneshot(put(
            r#"{"body":"x","format":"pdf"}"#,
            back["hash"].as_str().unwrap().to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(unknown.status(), StatusCode::BAD_REQUEST);
    let stale_delete = h
        .operator
        .oneshot(
            Request::delete("/api/docs/personal/ref-flip")
                .header("if-match", "stale")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stale_delete.status(), StatusCode::PRECONDITION_FAILED);
}
