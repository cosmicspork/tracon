//! A scripted fake of an OpenAI-compatible server, shared by the tests that
//! run the binary.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

pub struct Reply {
    pub status: StatusCode,
    pub body: String,
    /// Held this long before answering: a model still thinking.
    pub delay: Duration,
}

impl Reply {
    pub fn error(status: StatusCode, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
            delay: Duration::ZERO,
        }
    }

    pub fn after(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

#[derive(Clone, Default)]
pub struct Fake {
    /// Each request takes the next reply.
    script: Arc<Mutex<Vec<Reply>>>,
    pub requests: Arc<Mutex<Vec<Value>>>,
}

async fn completions(State(fake): State<Fake>, Json(body): Json<Value>) -> impl IntoResponse {
    fake.requests.lock().unwrap().push(body);
    let reply = {
        let mut script = fake.script.lock().unwrap();
        if script.is_empty() {
            Reply::error(StatusCode::INTERNAL_SERVER_ERROR, "script exhausted")
        } else {
            script.remove(0)
        }
    };
    tokio::time::sleep(reply.delay).await;
    (
        reply.status,
        [(header::CONTENT_TYPE, "text/event-stream")],
        reply.body,
    )
}

pub async fn serve(script: Vec<Reply>) -> (String, Fake) {
    let fake = Fake {
        script: Arc::new(Mutex::new(script)),
        ..Fake::default()
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(completions))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/v1"), fake)
}

pub fn sse(chunks: &[Value]) -> Reply {
    let mut out = String::new();
    for c in chunks {
        out.push_str(&format!("data: {c}\n\n"));
    }
    out.push_str("data: [DONE]\n\n");
    Reply::error(StatusCode::OK, out)
}

/// A tool call whose arguments arrive in two fragments, as llama.cpp streams them.
pub fn tool_call(id: &str, name: &str, args: Value) -> Reply {
    let args = args.to_string();
    let (a, b) = args.split_at(args.len() / 2);
    sse(&[
        json!({"id": format!("req-{id}"), "choices": [{"delta": {"tool_calls": [
            {"index": 0, "id": id, "type": "function", "function": {"name": name, "arguments": a}}
        ]}}]}),
        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": b}}]},
            "finish_reason": "tool_calls"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 100, "completion_tokens": 10}}),
    ])
}

pub fn answer(text: &str) -> Reply {
    sse(&[
        json!({"choices": [{"delta": {"content": text}, "finish_reason": "stop"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 120, "completion_tokens": 3,
            "prompt_tokens_details": {"cached_tokens": 100}}}),
    ])
}
