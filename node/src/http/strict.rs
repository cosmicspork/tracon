//! A JSON body read strictly: a key the request type does not have is
//! refused, naming it, rather than dropped without a word.
//!
//! Only at the HTTP edge. The types read here (`NewSession` above all) are
//! also read from what a peer forwards, where an unknown key is a field a
//! newer node added and must be ignored, so they never carry
//! `deny_unknown_fields` themselves.

use axum::{
    extract::{FromRequest, Request},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::de::{DeserializeOwned, Deserializer, Visitor};
use serde_json::{json, Value};

/// `Json<T>`, except that a top-level key `T` does not read is a 422 with
/// `error.kind = "unknown_field"` and one `error.fields` entry per key.
pub struct StrictJson<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for StrictJson<T> {
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Response> {
        let Json(value) = Json::<Value>::from_request(req, state)
            .await
            .map_err(IntoResponse::into_response)?;
        from_value(value)
            .map(StrictJson)
            .map_err(IntoResponse::into_response)
    }
}

/// A body refused as unreadable: always a 422, with the error object.
#[derive(Debug)]
pub struct Refusal(Value);

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": self.0 })),
        )
            .into_response()
    }
}

/// Reads `value` as `T`, refusing a top-level key `T` has no field for.
pub fn from_value<T: DeserializeOwned>(value: Value) -> Result<T, Refusal> {
    let keys: Vec<String> = match &value {
        Value::Object(map) => map.keys().cloned().collect(),
        _ => Vec::new(),
    };
    // The field names are the ones serde's derive hands the deserializer,
    // so there is no list here to keep in step with the struct.
    let mut known: &'static [&'static str] = &[];
    let read = T::deserialize(Fields {
        value,
        known: &mut known,
    });
    let unknown: Vec<&String> = keys
        .iter()
        .filter(|key| !known.is_empty() && !known.contains(&key.as_str()))
        .collect();
    if !unknown.is_empty() {
        return Err(refuse_unknown(&unknown, known));
    }
    read.map_err(|error| {
        Refusal(json!({
            "code": StatusCode::UNPROCESSABLE_ENTITY.as_u16(),
            "message": error.to_string(),
        }))
    })
}

fn refuse_unknown(unknown: &[&String], known: &[&str]) -> Refusal {
    let said = |key: &str| match suggest(key, known) {
        Some(near) => format!("unknown field \"{key}\"; did you mean \"{near}\"?"),
        None => format!("unknown field \"{key}\""),
    };
    let message = unknown
        .iter()
        .map(|key| said(key))
        .collect::<Vec<_>>()
        .join("; ");
    let fields: Vec<Value> = unknown
        .iter()
        .map(|key| {
            let mut field = json!({ "field": key, "message": said(key) });
            if let Some(near) = suggest(key, known) {
                field["suggestion"] = json!(near);
            }
            field
        })
        .collect();
    Refusal(json!({
        "code": StatusCode::UNPROCESSABLE_ENTITY.as_u16(),
        "kind": "unknown_field",
        "message": message,
        "fields": fields,
    }))
}

/// The known field `key` most likely meant: one it is a prefix of or that is
/// a prefix of it (`work_item` for `work_item_id`), else one within two
/// edits. None when nothing is that close.
fn suggest<'a>(key: &str, known: &[&'a str]) -> Option<&'a str> {
    let key = key.to_ascii_lowercase();
    let prefixed = known.iter().copied().find(|field| {
        let (short, long) = if field.len() < key.len() {
            (*field, key.as_str())
        } else {
            (key.as_str(), *field)
        };
        short.len() >= 4 && long.starts_with(short)
    });
    prefixed.or_else(|| {
        known
            .iter()
            .copied()
            .map(|field| (distance(&key, field), field))
            .filter(|(d, _)| *d <= 2)
            .min_by_key(|(d, _)| *d)
            .map(|(_, field)| field)
    })
}

/// Levenshtein distance, by characters.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (above + 1)
                .min(row[j] + 1)
                .min(diagonal + usize::from(ca != *cb));
            diagonal = above;
        }
    }
    row[b.len()]
}

/// A `Value` deserializer that notes the field names a struct asks for.
struct Fields<'a> {
    value: Value,
    known: &'a mut &'static [&'static str],
}

impl<'de> Deserializer<'de> for Fields<'_> {
    type Error = serde_json::Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.value.deserialize_any(visitor)
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        *self.known = fields;
        self.value.deserialize_struct(name, fields, visitor)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map enum identifier ignored_any
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::NewSession;

    async fn body(refusal: Refusal) -> Value {
        let response = refusal.into_response();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn base() -> Value {
        json!({ "channel": "personal", "repo_path": "forge.invalid/o/r" })
    }

    #[tokio::test]
    async fn an_unknown_key_is_refused_by_name_with_the_field_it_meant() {
        let mut v = base();
        v["work_item"] = json!("w1");
        let body = body(from_value::<NewSession>(v).unwrap_err()).await;
        assert_eq!(body["error"]["kind"], "unknown_field", "{body}");
        assert_eq!(
            body["error"]["message"],
            "unknown field \"work_item\"; did you mean \"work_item_id\"?"
        );
        assert_eq!(body["error"]["fields"][0]["field"], "work_item");
        assert_eq!(body["error"]["fields"][0]["suggestion"], "work_item_id");
    }

    #[tokio::test]
    async fn an_unknown_key_is_named_before_a_missing_one_it_stood_for() {
        // `chanel` for `channel`: the missing field is the misspelt one.
        let v = json!({ "chanel": "personal", "repo_path": "forge.invalid/o/r" });
        let body = body(from_value::<NewSession>(v).unwrap_err()).await;
        assert_eq!(
            body["error"]["message"],
            "unknown field \"chanel\"; did you mean \"channel\"?"
        );
    }

    #[tokio::test]
    async fn a_key_nothing_is_close_to_is_named_without_a_guess() {
        let mut v = base();
        v["colour"] = json!("blue");
        let body = body(from_value::<NewSession>(v).unwrap_err()).await;
        assert_eq!(body["error"]["message"], "unknown field \"colour\"");
        assert!(body["error"]["fields"][0].get("suggestion").is_none());
    }

    #[test]
    fn every_field_of_a_full_session_is_known() {
        let mut v = base();
        for key in [
            "branch",
            "base",
            "work_item_id",
            "initial_prompt",
            "node_id",
            "review_id",
            "base_sha",
            "workspace_id",
            "parent_session",
            "continued_from",
            "harness",
        ] {
            v[key] = json!("x");
        }
        v["model"] = json!("m/a");
        v["budget_tokens"] = json!(10);
        v["phase"] = json!("plan");
        v["told_reviews"] = json!(["r"]);
        v["on_exhaustion"] = Value::Null;
        let spec: NewSession = from_value(v).unwrap();
        assert_eq!(spec.work_item_id.as_deref(), Some("x"));
        // And what the node writes, read back, is read strictly too.
        let again = serde_json::to_value(&spec).unwrap();
        assert!(from_value::<NewSession>(again).is_ok());
    }

    #[test]
    fn a_session_read_without_the_edge_still_ignores_a_newer_field() {
        // What a peer forwards: a newer node's extra field is not a refusal.
        let mut v = base();
        v["a_field_from_a_newer_node"] = json!(true);
        let spec: NewSession = serde_json::from_value(v).unwrap();
        assert_eq!(spec.channel, "personal");
    }

    #[test]
    fn suggestions_are_close_or_absent() {
        let known = &["work_item_id", "channel", "repo_path", "model"];
        assert_eq!(suggest("work_item", known), Some("work_item_id"));
        assert_eq!(suggest("modle", known), Some("model"));
        assert_eq!(suggest("repo", known), Some("repo_path"));
        assert_eq!(suggest("xyz", known), None);
    }
}
