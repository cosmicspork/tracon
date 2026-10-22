//! A scripted fake of an OpenAI-compatible server, shared by the tests that
//! run the binary.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

pub struct Reply {
    pub status: StatusCode,
    pub body: String,
    /// Held this long before answering: a model still thinking.
    pub delay: Duration,
    pub headers: Vec<(&'static str, String)>,
    /// Send the body, then drop the connection instead of ending it.
    pub cut: bool,
}

impl Reply {
    pub fn error(status: StatusCode, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
            delay: Duration::ZERO,
            headers: Vec::new(),
            cut: false,
        }
    }

    /// A streamed answer that breaks off after these chunks: no finish, no
    /// `[DONE]`, and the connection dropped.
    pub fn cut_after(chunks: &[Value]) -> Self {
        let mut body = String::new();
        for c in chunks {
            body.push_str(&format!("data: {c}\n\n"));
        }
        Self {
            cut: true,
            ..Self::error(StatusCode::OK, body)
        }
    }

    pub fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
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

/// In a reply's body, replaced by a quarter of the request's bytes: a prompt
/// count that grows with the conversation, as a real server's does.
pub const MEASURED: &str = "\"$measured\"";

async fn completions(State(fake): State<Fake>, Json(body): Json<Value>) -> Response {
    let measured = (body.to_string().len() / 4).to_string();
    fake.requests.lock().unwrap().push(body);
    let mut reply = {
        let mut script = fake.script.lock().unwrap();
        if script.is_empty() {
            Reply::error(StatusCode::INTERNAL_SERVER_ERROR, "script exhausted")
        } else {
            script.remove(0)
        }
    };
    reply.body = reply.body.replace(MEASURED, &measured);
    tokio::time::sleep(reply.delay).await;
    let body = if reply.cut {
        use futures_util::StreamExt;
        let chunks: Vec<Result<String, std::io::Error>> = vec![
            Ok(reply.body),
            Err(std::io::Error::other("connection dropped")),
        ];
        // The drop waits a moment, so what came before it reaches the client
        // rather than being lost with the connection.
        Body::from_stream(futures_util::stream::iter(chunks).then(|c| async move {
            if c.is_err() {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            c
        }))
    } else {
        Body::from(reply.body)
    };
    let mut response = (
        reply.status,
        [(header::CONTENT_TYPE, "text/event-stream")],
        body,
    )
        .into_response();
    for (name, value) in reply.headers {
        response.headers_mut().insert(
            HeaderName::from_static(name),
            HeaderValue::from_str(&value).unwrap(),
        );
    }
    response
}

pub async fn serve(script: Vec<Reply>) -> (String, Fake) {
    let fake = Fake {
        script: Arc::new(Mutex::new(script)),
        ..Fake::default()
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(completions))
        .route("/v1/responses", post(completions))
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

/// Several tool calls in one response, each whole.
pub fn tool_calls(calls: &[(&str, &str, Value)]) -> Reply {
    let deltas: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(i, (id, name, args))| {
            json!({"index": i, "id": id, "type": "function",
                "function": {"name": name, "arguments": args.to_string()}})
        })
        .collect();
    sse(&[
        json!({"id": format!("req-{}", calls[0].0), "choices": [{"delta": {"tool_calls": deltas},
            "finish_reason": "tool_calls"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 100, "completion_tokens": 10}}),
    ])
}

/// A tool call whose prompt count is the request's size.
pub fn measured_call(id: &str, name: &str, args: Value) -> Reply {
    let mut reply = tool_call(id, name, args);
    reply.body = reply.body.replace(
        "\"prompt_tokens\":100",
        &format!("\"prompt_tokens\":{MEASURED}"),
    );
    reply
}

pub fn answer(text: &str) -> Reply {
    sse(&[
        json!({"choices": [{"delta": {"content": text}, "finish_reason": "stop"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 120, "completion_tokens": 3,
            "prompt_tokens_details": {"cached_tokens": 100}}}),
    ])
}

/// A Responses stream: these events, each with its `type`, then nothing more.
pub fn events(events: &[Value]) -> Reply {
    let mut out = String::new();
    for e in events {
        out.push_str(&format!(
            "event: {}\ndata: {e}\n\n",
            e["type"].as_str().unwrap_or("")
        ));
    }
    Reply::error(StatusCode::OK, out)
}

fn completed(id: &str, output: Value) -> Value {
    json!({"type": "response.completed", "response": {"id": id, "status": "completed",
        "output": output,
        "usage": {"input_tokens": 100, "input_tokens_details": {"cached_tokens": 60},
            "output_tokens": 10, "output_tokens_details": {"reasoning_tokens": 4}}}})
}

/// A Responses turn that thinks, then calls one tool: the reasoning item
/// carries `encrypted` as its content.
pub fn responses_call(id: &str, name: &str, args: Value, encrypted: &str) -> Reply {
    let reasoning = json!({"id": format!("rs_{id}"), "type": "reasoning",
        "summary": [{"type": "summary_text", "text": "thinking"}], "encrypted_content": encrypted});
    let call = json!({"id": format!("fc_{id}"), "type": "function_call", "status": "completed",
        "call_id": id, "name": name, "arguments": args.to_string()});
    events(&[
        json!({"type": "response.created", "response": {"id": format!("resp_{id}"), "status": "in_progress"}}),
        json!({"type": "response.reasoning_summary_text.delta", "delta": "thinking"}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": reasoning}),
        json!({"type": "response.function_call_arguments.delta", "delta": args.to_string()}),
        json!({"type": "response.output_item.done", "output_index": 1, "item": call}),
        completed(&format!("resp_{id}"), json!([reasoning, call])),
    ])
}

pub fn responses_answer(text: &str) -> Reply {
    let message = json!({"id": "msg_1", "type": "message", "role": "assistant", "status": "completed",
        "content": [{"type": "output_text", "text": text, "annotations": []}]});
    events(&[
        json!({"type": "response.created", "response": {"id": "resp_answer", "status": "in_progress"}}),
        json!({"type": "response.output_text.delta", "delta": text}),
        json!({"type": "response.output_item.done", "output_index": 0, "item": message}),
        completed("resp_answer", json!([message])),
    ])
}
