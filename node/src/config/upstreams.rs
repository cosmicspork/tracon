//! `[upstreams]`: hosts the node attaches a stored credential to on a
//! client's behalf, so the client never holds it.
//!
//! A client inside the boundary (a repository's default-branch warm-up, or a
//! session whose upstream says so) sends a plain `http://<host>/…` request
//! through its own egress grant. The node, and only the node, opens TLS to
//! `https://<host>/…`, writes the credential's header and forwards the answer
//! back. Nothing is intercepted: the TLS the node speaks is its own, to the
//! one host the operator declared, and a CONNECT tunnel to any host stays
//! end to end and uncredentialed.

use serde::{Deserialize, Serialize};

/// The `[upstreams]` table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Upstreams {
    /// How a `[[repo]]` entry's `prepare_credentials` reach its default
    /// branch's warm-up: `env` lends each credential's environment to the
    /// preparation's container; `attach` lends nothing and attaches each
    /// through the upstream declared for it below.
    pub preparation: PreparationLending,
    /// `[[upstreams.host]]`: one declared upstream each.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub host: Vec<Upstream>,
}

/// How preparation is lent its credentials.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PreparationLending {
    /// The credential's environment in the preparation's container.
    #[default]
    Env,
    /// Nothing in the container; the node attaches the credential to
    /// requests for its declared host.
    Attach,
}

/// One host the node attaches a credential to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Upstream {
    /// The host, exactly: `nova.laravel.com`. Never a pattern, a port or a
    /// path; reached on 443 over the node's own TLS.
    pub host: String,
    /// The broker credential (kind `env`) whose values are attached.
    pub credential: String,
    /// How: `basic` (`username` and `password`), `header` (`header` set to
    /// `value`), or `composer` (the credential's `COMPOSER_AUTH`, whose
    /// `http-basic` or `bearer` entry for `host` is used).
    pub attach: Attach,
    /// For `header`: the header to set. `Authorization` when left out.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub header: String,
    /// For `header`: its value, naming the credential's variables as
    /// `${NAME}`: `Bearer ${TOKEN}`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub value: String,
    /// For `basic`: the user name, literal or `${NAME}`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub username: String,
    /// For `basic`: the password, as `${NAME}`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub password: String,
    /// Attach it for sessions too, in channels the credential is bound to.
    /// Off unless asked for: a session runs what a model decides, and with
    /// this it can make any request of `host` as the credential's owner,
    /// though it never reads the credential.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub sessions: bool,
}

/// How a declared upstream's credential is written into a request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Attach {
    #[default]
    Basic,
    Header,
    Composer,
}

/// The header an `attach = "header"` upstream sets when it names none.
pub const DEFAULT_HEADER: &str = "authorization";

/// Headers an upstream may not set: the request's framing, its route, and
/// what is said to the proxy rather than to the host.
const RESERVED_HEADERS: &[&str] = &[
    "host",
    "connection",
    "content-length",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "keep-alive",
    "proxy-authorization",
    "proxy-authenticate",
    "proxy-connection",
];

impl Upstream {
    /// The header this upstream writes, lowercase.
    pub fn header_name(&self) -> String {
        match self.attach {
            Attach::Header if !self.header.trim().is_empty() => self.header.trim().to_lowercase(),
            _ => DEFAULT_HEADER.into(),
        }
    }
}

impl Upstreams {
    /// The upstreams declared for `credential`.
    pub fn for_credential<'a>(&'a self, credential: &'a str) -> impl Iterator<Item = &'a Upstream> {
        self.host
            .iter()
            .filter(move |upstream| upstream.credential == credential)
    }
}

/// The `${NAME}` references in a template, in order, or why it is not one.
pub fn template_names(template: &str) -> Result<Vec<&str>, String> {
    let mut names = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| format!("{template:?} opens a ${{ it does not close"))?;
        let name = &after[..end];
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err(format!(
                "{template:?} names {name:?}, which is not a variable name"
            ));
        }
        names.push(name);
        rest = &after[end + 1..];
    }
    Ok(names)
}

/// Refuse an `[upstreams]` table the node could not honour, at load rather
/// than at the request it would fail.
pub fn validate_upstreams(upstreams: &Upstreams) -> Result<(), String> {
    let mut seen: Vec<&str> = Vec::new();
    for upstream in &upstreams.host {
        let host = upstream.host.as_str();
        if !super::literal_host(host) {
            return Err(format!(
                "upstreams.host {host:?} is not a host name: write it lowercase, with no scheme, \
                 port, path or pattern"
            ));
        }
        if seen.contains(&host) {
            return Err(format!(
                "upstreams.host {host} is declared twice: one host takes one credential"
            ));
        }
        seen.push(host);
        let credential = upstream.credential.as_str();
        if credential.trim().is_empty() || credential.trim() != credential {
            return Err(format!(
                "upstreams.host {host}: credential names a credential by an empty or padded name"
            ));
        }
        let unused = |field: &str, value: &str| -> Result<(), String> {
            match value.is_empty() {
                true => Ok(()),
                false => Err(format!(
                    "upstreams.host {host}: {field} means nothing with attach = \"{}\"",
                    serde_json::to_value(upstream.attach)
                        .ok()
                        .and_then(|value| value.as_str().map(str::to_string))
                        .unwrap_or_default()
                )),
            }
        };
        match upstream.attach {
            Attach::Basic => {
                unused("header", &upstream.header)?;
                unused("value", &upstream.value)?;
                if upstream.username.is_empty() || upstream.password.is_empty() {
                    return Err(format!(
                        "upstreams.host {host}: attach = \"basic\" needs username and password"
                    ));
                }
                template_names(&upstream.username)
                    .map_err(|e| format!("upstreams.host {host}: username {e}"))?;
                // A literal password would be a secret in node.toml.
                if template_names(&upstream.password)
                    .map_err(|e| format!("upstreams.host {host}: password {e}"))?
                    .is_empty()
                {
                    return Err(format!(
                        "upstreams.host {host}: password must name the credential's variable \
                         as ${{NAME}}; node.toml holds no secrets"
                    ));
                }
            }
            Attach::Header => {
                unused("username", &upstream.username)?;
                unused("password", &upstream.password)?;
                let name = upstream.header_name();
                if hyper::header::HeaderName::from_bytes(name.as_bytes()).is_err() {
                    return Err(format!(
                        "upstreams.host {host}: header {name:?} is not a header name"
                    ));
                }
                if RESERVED_HEADERS.contains(&name.as_str()) {
                    return Err(format!(
                        "upstreams.host {host}: header {name} is the request's own, not a \
                         credential's"
                    ));
                }
                if template_names(&upstream.value)
                    .map_err(|e| format!("upstreams.host {host}: value {e}"))?
                    .is_empty()
                {
                    return Err(format!(
                        "upstreams.host {host}: value must name the credential's variable as \
                         ${{NAME}}; node.toml holds no secrets"
                    ));
                }
            }
            Attach::Composer => {
                for (field, value) in [
                    ("header", &upstream.header),
                    ("value", &upstream.value),
                    ("username", &upstream.username),
                    ("password", &upstream.password),
                ] {
                    unused(field, value)?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nova() -> Upstream {
        Upstream {
            host: "nova.laravel.com".into(),
            credential: "nova".into(),
            attach: Attach::Composer,
            ..Default::default()
        }
    }

    fn with(hosts: Vec<Upstream>) -> Upstreams {
        Upstreams {
            host: hosts,
            ..Default::default()
        }
    }

    #[test]
    fn a_declared_upstream_is_a_host_a_credential_and_how() {
        validate_upstreams(&with(vec![nova()])).unwrap();
        validate_upstreams(&with(vec![Upstream {
            host: "npm.pkg.github.com".into(),
            credential: "npm".into(),
            attach: Attach::Header,
            value: "Bearer ${NPM_TOKEN}".into(),
            ..Default::default()
        }]))
        .unwrap();
        validate_upstreams(&with(vec![Upstream {
            attach: Attach::Basic,
            username: "ops@example.com".into(),
            password: "${PASSWORD}".into(),
            ..nova()
        }]))
        .unwrap();
    }

    #[test]
    fn anything_but_a_literal_host_is_refused() {
        for host in [
            "",
            "Nova.laravel.com",
            "https://nova.laravel.com",
            "nova.laravel.com:443",
            "nova.laravel.com/dist",
            "*.laravel.com",
            "localhost",
        ] {
            let error = validate_upstreams(&with(vec![Upstream {
                host: host.into(),
                ..nova()
            }]))
            .unwrap_err();
            assert!(error.contains("not a host name"), "{host}: {error}");
        }
        let error = validate_upstreams(&with(vec![nova(), nova()])).unwrap_err();
        assert!(error.contains("declared twice"), "{error}");
    }

    #[test]
    fn a_secret_is_named_never_written() {
        let error = validate_upstreams(&with(vec![Upstream {
            attach: Attach::Header,
            value: "Bearer sk-live-123".into(),
            ..nova()
        }]))
        .unwrap_err();
        assert!(error.contains("holds no secrets"), "{error}");
        let error = validate_upstreams(&with(vec![Upstream {
            attach: Attach::Basic,
            username: "ops".into(),
            password: "hunter22".into(),
            ..nova()
        }]))
        .unwrap_err();
        assert!(error.contains("holds no secrets"), "{error}");
    }

    #[test]
    fn a_header_that_frames_or_routes_the_request_is_refused() {
        for header in [
            "Host",
            "Proxy-Authorization",
            "content-length",
            "bad header",
        ] {
            let error = validate_upstreams(&with(vec![Upstream {
                attach: Attach::Header,
                header: header.into(),
                value: "${TOKEN}".into(),
                ..nova()
            }]))
            .unwrap_err();
            assert!(error.contains("header"), "{header}: {error}");
        }
    }

    #[test]
    fn a_field_the_attach_kind_does_not_read_is_refused() {
        let error = validate_upstreams(&with(vec![Upstream {
            value: "${TOKEN}".into(),
            ..nova()
        }]))
        .unwrap_err();
        assert!(error.contains("means nothing"), "{error}");
    }

    #[test]
    fn a_template_names_variables() {
        assert_eq!(
            template_names("Bearer ${A_1} and ${B}").unwrap(),
            ["A_1", "B"]
        );
        assert!(template_names("plain").unwrap().is_empty());
        assert!(template_names("${").is_err());
        assert!(template_names("${a-b}").is_err());
        assert!(template_names("${}").is_err());
    }

    #[test]
    fn the_table_reads_as_written() {
        let parsed: Upstreams = toml::from_str(
            r#"
            preparation = "attach"
            [[host]]
            host = "nova.laravel.com"
            credential = "nova"
            attach = "composer"
            "#,
        )
        .unwrap();
        assert_eq!(parsed.preparation, PreparationLending::Attach);
        assert_eq!(parsed.host, [nova()]);
        assert_eq!(Upstreams::default().preparation, PreparationLending::Env);
    }
}
