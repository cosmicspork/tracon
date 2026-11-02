//! A declared upstream's credential as the node writes it into a request
//! (`config::upstreams`). Built here, inside the broker, for the same reason
//! the model gateway's injection is: no accessor hands a secret out as a
//! value anything else could put somewhere it should not be. What leaves is
//! a header bound to one host, marked sensitive, that prints as redacted.

use std::path::Path;

use base64::Engine;
use hyper::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;

use super::{Broker, BrokerError, KIND_ENV, LENT_TO_PREPARATION};
use crate::config::upstreams::{template_names, Attach, Upstream, Upstreams};

/// One credential, rendered as the header it is attached as, for one host.
#[derive(Clone)]
pub struct Attachment {
    host: String,
    credential: String,
    name: HeaderName,
    value: HeaderValue,
}

impl std::fmt::Debug for Attachment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Attachment")
            .field("host", &self.host)
            .field("credential", &self.credential)
            .field("header", &self.name)
            .field("value", &"[redacted]")
            .finish()
    }
}

impl Attachment {
    /// The one host it is attached for, lowercase.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The broker credential it was rendered from.
    pub fn credential(&self) -> &str {
        &self.credential
    }

    /// The header it sets, which a request's own copy of is dropped first.
    pub fn header(&self) -> &HeaderName {
        &self.name
    }

    /// Write it into `headers`, replacing whatever the client put there.
    pub(crate) fn apply(&self, headers: &mut HeaderMap) {
        headers.insert(self.name.clone(), self.value.clone());
    }

    /// An attachment carrying `value` as is. For tests, which have no
    /// broker to render one from.
    #[doc(hidden)]
    pub fn for_test(host: &str, header: &str, value: &str) -> Self {
        let mut value = HeaderValue::from_str(value).expect("a header value");
        value.set_sensitive(true);
        Self {
            host: host.to_ascii_lowercase(),
            credential: "test".into(),
            name: HeaderName::from_bytes(header.as_bytes()).expect("a header name"),
            value,
        }
    }
}

impl Broker {
    /// `upstream`'s credential, for the warm-up of `repo`'s base cache: bound
    /// as [`Broker::preparation_env`] binds it, by the repository rather than
    /// a channel, and only an `env` credential.
    pub fn attach_for_preparation(
        &self,
        upstream: &Upstream,
        repo: &Path,
        node_id: &str,
    ) -> Result<Attachment, BrokerError> {
        let env = self.preparation_env(&upstream.credential, repo, node_id)?;
        render(upstream, &env)
    }

    /// `upstream`'s credential, for a session in `channel`: bound as
    /// [`Broker::env_for`] binds it, and only an `env` credential, so a model
    /// credential never goes anywhere but its provider.
    pub fn attach_for_session(
        &self,
        upstream: &Upstream,
        channel: &str,
        node_id: &str,
    ) -> Result<Attachment, BrokerError> {
        let cred = self.usable(&upstream.credential, channel, node_id)?;
        if cred.kind != KIND_ENV {
            return Err(BrokerError::NotEnv {
                name: upstream.credential.clone(),
            });
        }
        render(upstream, &cred.env)
    }

    /// Every upstream that opts sessions in and whose credential `channel`
    /// may use on this node. One that does not apply is left out, not
    /// refused: a channel the credential is not bound to is simply not one
    /// it is attached for.
    pub fn session_attachments(
        &self,
        upstreams: &Upstreams,
        channel: &str,
        node_id: &str,
    ) -> Vec<Attachment> {
        upstreams
            .host
            .iter()
            .filter(|upstream| upstream.sessions)
            .filter_map(
                |upstream| match self.attach_for_session(upstream, channel, node_id) {
                    Ok(attachment) => Some(attachment),
                    Err(
                        BrokerError::NotBound { .. }
                        | BrokerError::NotOnThisNode { .. }
                        | BrokerError::Unknown(_),
                    ) => None,
                    Err(error) => {
                        tracing::warn!(host = %upstream.host, %error, "upstream not attached for the session");
                        None
                    }
                },
            )
            .collect()
    }
}

fn unusable(upstream: &Upstream, reason: impl Into<String>) -> BrokerError {
    BrokerError::Upstream {
        name: upstream.credential.clone(),
        host: upstream.host.clone(),
        reason: reason.into(),
    }
}

/// `template` with each `${NAME}` replaced by the credential's `NAME`.
fn fill(
    upstream: &Upstream,
    field: &str,
    template: &str,
    env: &std::collections::BTreeMap<String, String>,
) -> Result<String, BrokerError> {
    let names = template_names(template).map_err(|error| unusable(upstream, error))?;
    let mut out = template.to_string();
    for name in names {
        let value = env.get(name).ok_or_else(|| {
            unusable(
                upstream,
                format!("its {field} names ${{{name}}}, which the credential does not set"),
            )
        })?;
        out = out.replacen(&format!("${{{name}}}"), value, 1);
    }
    Ok(out)
}

fn basic(username: &str, password: &str) -> String {
    let pair = format!("{username}:{password}");
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(pair)
    )
}

/// The header value Composer would send `host` from a `COMPOSER_AUTH`
/// document: its `http-basic` entry, else its `bearer` one.
fn from_composer_auth(upstream: &Upstream, document: &str) -> Result<String, BrokerError> {
    let parsed: Value = serde_json::from_str(document)
        .map_err(|_| unusable(upstream, "its COMPOSER_AUTH is not JSON"))?;
    if let Some(entry) = parsed["http-basic"].get(&upstream.host) {
        let (Some(username), Some(password)) =
            (entry["username"].as_str(), entry["password"].as_str())
        else {
            return Err(unusable(
                upstream,
                "its COMPOSER_AUTH http-basic entry needs a username and a password",
            ));
        };
        return Ok(basic(username, password));
    }
    if let Some(token) = parsed["bearer"].get(&upstream.host).and_then(Value::as_str) {
        return Ok(format!("Bearer {token}"));
    }
    Err(unusable(
        upstream,
        "its COMPOSER_AUTH has no http-basic or bearer entry for the host",
    ))
}

fn render(
    upstream: &Upstream,
    env: &std::collections::BTreeMap<String, String>,
) -> Result<Attachment, BrokerError> {
    let rendered = match upstream.attach {
        Attach::Basic => basic(
            &fill(upstream, "username", &upstream.username, env)?,
            &fill(upstream, "password", &upstream.password, env)?,
        ),
        Attach::Header => fill(upstream, "value", &upstream.value, env)?,
        Attach::Composer => {
            let document = env.get("COMPOSER_AUTH").ok_or_else(|| {
                unusable(
                    upstream,
                    "attach = \"composer\" reads COMPOSER_AUTH, which the credential does not set",
                )
            })?;
            from_composer_auth(upstream, document)?
        }
    };
    let name = HeaderName::from_bytes(upstream.header_name().as_bytes())
        .map_err(|_| unusable(upstream, "its header is not a header name"))?;
    let mut value = HeaderValue::from_str(&rendered)
        .map_err(|_| unusable(upstream, "its value is not one a header can carry"))?;
    value.set_sensitive(true);
    Ok(Attachment {
        host: upstream.host.to_ascii_lowercase(),
        credential: upstream.credential.clone(),
        name,
        value,
    })
}

/// What `names` attach for `repo`'s default-branch warm-up, through the
/// upstreams declared for them, read from the broker as it is now. Every
/// credential named must have at least one declared upstream: with
/// preparation lent this way nothing is lent as environment, so one with
/// none would silently reach its registry with nothing.
pub fn preparation_attachments_for(
    repo: &Path,
    names: &[String],
    upstreams: &Upstreams,
) -> Result<Vec<Attachment>, String> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let (broker, node_id) = LENT_TO_PREPARATION
        .get()
        .ok_or("the node's credential broker is not loaded in this process")?;
    let broker = broker
        .read()
        .map_err(|_| "the credential broker is unavailable".to_string())?;
    attachments(&broker, repo, names, upstreams, node_id)
}

fn attachments(
    broker: &Broker,
    repo: &Path,
    names: &[String],
    upstreams: &Upstreams,
    node_id: &str,
) -> Result<Vec<Attachment>, String> {
    let mut out = Vec::new();
    for name in names {
        let declared: Vec<&Upstream> = upstreams.for_credential(name).collect();
        if declared.is_empty() {
            return Err(format!(
                "credential {name} has no declared upstream; with [upstreams] preparation = \
                 \"attach\" a credential reaches preparation only through an [[upstreams.host]] \
                 naming it"
            ));
        }
        for upstream in declared {
            out.push(
                broker
                    .attach_for_preparation(upstream, repo, node_id)
                    .map_err(|error| error.to_string())?,
            );
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LICENCE: &str = "nova-licence-key";

    fn broker() -> Broker {
        toml::from_str(&format!(
            r#"
            [credentials.nova]
            repos = ["/src/shop"]
            channels = ["work"]
            [credentials.nova.env]
            COMPOSER_AUTH = '{{"http-basic":{{"nova.laravel.com":{{"username":"ops@example.com","password":"{LICENCE}"}}}}}}'
            USER = "ops@example.com"
            PASSWORD = "{LICENCE}"
            TOKEN = "tok-123456"

            [credentials.model]
            kind = "api_key"
            provider = "anthropic"
            channels = ["work"]
            repos = ["/src/shop"]
            [credentials.model.env]
            TOKEN = "sk-x"
            "#
        ))
        .unwrap()
    }

    fn nova(attach: Attach) -> Upstream {
        Upstream {
            host: "nova.laravel.com".into(),
            credential: "nova".into(),
            attach,
            ..Default::default()
        }
    }

    fn header_of(attachment: &Attachment) -> String {
        let mut headers = HeaderMap::new();
        attachment.apply(&mut headers);
        assert!(headers[attachment.header()].is_sensitive());
        headers[attachment.header()].to_str().unwrap().to_string()
    }

    fn decoded(header: &str) -> String {
        let encoded = header.strip_prefix("Basic ").unwrap();
        String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn composer_auth_is_read_for_its_own_host() {
        let shop = Path::new("/src/shop");
        let attached = broker()
            .attach_for_preparation(&nova(Attach::Composer), shop, "node-a")
            .unwrap();
        assert_eq!(attached.host(), "nova.laravel.com");
        assert_eq!(attached.header(), "authorization");
        assert_eq!(
            decoded(&header_of(&attached)),
            format!("ops@example.com:{LICENCE}")
        );
        // An entry for another host is not this one's.
        let other = Upstream {
            host: "satis.example.com".into(),
            ..nova(Attach::Composer)
        };
        let error = broker()
            .attach_for_preparation(&other, shop, "node-a")
            .unwrap_err();
        assert!(error.to_string().contains("no http-basic or bearer entry"));
    }

    #[test]
    fn basic_and_header_fill_the_credential_s_variables() {
        let shop = Path::new("/src/shop");
        let basic = Upstream {
            username: "${USER}".into(),
            password: "${PASSWORD}".into(),
            ..nova(Attach::Basic)
        };
        let attached = broker()
            .attach_for_preparation(&basic, shop, "node-a")
            .unwrap();
        assert_eq!(
            decoded(&header_of(&attached)),
            format!("ops@example.com:{LICENCE}")
        );
        let header = Upstream {
            header: "X-Api-Key".into(),
            value: "Token ${TOKEN}".into(),
            ..nova(Attach::Header)
        };
        let attached = broker()
            .attach_for_preparation(&header, shop, "node-a")
            .unwrap();
        assert_eq!(attached.header(), "x-api-key");
        assert_eq!(header_of(&attached), "Token tok-123456");
        let missing = Upstream {
            value: "Bearer ${NOPE}".into(),
            ..header
        };
        let error = broker()
            .attach_for_preparation(&missing, shop, "node-a")
            .unwrap_err();
        assert!(error.to_string().contains("${NOPE}"), "{error}");
    }

    #[test]
    fn it_is_bound_as_lending_is_and_never_printed() {
        let b = broker();
        let elsewhere = b
            .attach_for_preparation(&nova(Attach::Composer), Path::new("/src/other"), "node-a")
            .unwrap_err();
        assert!(matches!(elsewhere, BrokerError::NotForRepo { .. }));
        let model = Upstream {
            credential: "model".into(),
            attach: Attach::Header,
            value: "${TOKEN}".into(),
            ..nova(Attach::Header)
        };
        assert!(matches!(
            b.attach_for_preparation(&model, Path::new("/src/shop"), "node-a"),
            Err(BrokerError::NotEnv { .. })
        ));
        assert!(matches!(
            b.attach_for_session(&model, "work", "node-a"),
            Err(BrokerError::NotEnv { .. })
        ));
        let attached = b
            .attach_for_preparation(&nova(Attach::Composer), Path::new("/src/shop"), "node-a")
            .unwrap();
        let printed = format!("{attached:?}");
        assert!(!printed.contains(LICENCE) && printed.contains("[redacted]"));
        // Nor does a refusal carry a value.
        let error = b
            .attach_for_preparation(
                &Upstream {
                    host: "satis.example.com".into(),
                    ..nova(Attach::Composer)
                },
                Path::new("/src/shop"),
                "node-a",
            )
            .unwrap_err()
            .to_string();
        assert!(!error.contains(LICENCE), "{error}");
    }

    #[test]
    fn a_session_is_attached_only_what_opts_sessions_in_for_its_channel() {
        let b = broker();
        let upstreams = Upstreams {
            host: vec![
                Upstream {
                    sessions: true,
                    ..nova(Attach::Composer)
                },
                Upstream {
                    host: "api.example.com".into(),
                    value: "${TOKEN}".into(),
                    ..nova(Attach::Header)
                },
            ],
            ..Default::default()
        };
        let attached = b.session_attachments(&upstreams, "work", "node-a");
        assert_eq!(
            attached.iter().map(Attachment::host).collect::<Vec<_>>(),
            ["nova.laravel.com"]
        );
        assert!(b
            .session_attachments(&upstreams, "personal", "node-a")
            .is_empty());
    }

    #[test]
    fn a_warm_up_needs_an_upstream_for_every_credential_it_names() {
        let b = broker();
        let upstreams = Upstreams {
            host: vec![nova(Attach::Composer)],
            ..Default::default()
        };
        let shop = Path::new("/src/shop");
        let attached = attachments(&b, shop, &["nova".into()], &upstreams, "node-a").unwrap();
        assert_eq!(attached.len(), 1);
        let error = attachments(
            &b,
            shop,
            &["nova".into(), "npm".into()],
            &upstreams,
            "node-a",
        )
        .unwrap_err();
        assert!(error.contains("npm has no declared upstream"), "{error}");
    }
}
