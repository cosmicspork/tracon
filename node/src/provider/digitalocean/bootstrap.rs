//! How a DigitalOcean sandbox learns where the node is and which token to
//! present, without that token ever being stored by DigitalOcean.
//!
//! A session's manifest `env` is kept server-side in the agent config the
//! create stores, which outlives the session until it is deleted and which
//! anyone holding the team's API token can read back. So the manifest
//! carries nothing of the run: no address, no pin, no token. Once the
//! session is ready the node runs one `exec` in it that writes the bootstrap
//! into `/run/tracon` (mode 0600, root's), adds the gateway's name to
//! `/etc/hosts`, and leaves the rest to the template's `tracon-sandbox`
//! service ([`RUN_SCRIPT`]), which waits for the bootstrap, reads it, and
//! runs the supervisor as its child under DigitalOcean's own init.
//!
//! The token still travels in that exec's arguments, which DigitalOcean may
//! log. Such a copy is worth nothing: the token is redeemed by the
//! sandbox's first connection (seconds after the exec), it is refused once
//! its window has passed unused (`TokenRegistry::issue_within`), and the
//! run's tokens are revoked when it ends.

/// Where the bootstrap is written inside the sandbox.
pub const DIR: &str = "/run/tracon";

/// The template's `tracon-sandbox` service: an s6 `run` script (also what
/// the bootstrap starts itself where the image has no such service). It
/// waits for the node's bootstrap, removes it once read, and execs the
/// supervisor, which removes the token file once read. s6 starts it again
/// when the supervisor exits; it then waits for a bootstrap that only a new
/// run would bring.
pub const RUN_SCRIPT: &str = r#"#!/bin/sh
while [ ! -s /run/tracon/bootstrap ]; do sleep 0.2; done
set -a
. /run/tracon/bootstrap
set +a
rm -f /run/tracon/bootstrap
export TRACON_SANDBOX_TOKEN_FILE=/run/tracon/token
exec /opt/tracon/bin/tracon-sandbox
"#;

/// Where the template's image installs [`RUN_SCRIPT`] as an s6 service.
pub const SERVICE_DIR: &str = "/etc/services.d/tracon-sandbox";

/// The exec that delivers a run's bootstrap. Positional arguments: the
/// variable the dial goes in (`ADDR` or `RELAY_URL`), the dial, the
/// certificate pin, the server name, the token, the gateway's name and the
/// run script. The token is written before the bootstrap that names it, so
/// the service never starts without one.
///
/// The runtime's proxy settings are in this exec's environment but need not
/// be in the service's, so a WebSocket dial gets a copy under the
/// supervisor's own names, which it removes from its environment like the
/// rest: nothing it starts sees anything new. A value with a character that
/// could leave its quotes is left out.
const DELIVER: &str = r#"set -eu
umask 077
mkdir -p /run/tracon
grep -qs "^127.0.0.1 $6\$" /etc/hosts || printf '127.0.0.1 %s\n' "$6" >> /etc/hosts
printf '%s' "$5" > /run/tracon/token.new
mv -f /run/tracon/token.new /run/tracon/token
case "$1" in
  ADDR|RELAY_URL) ;;
  *) echo "unknown dial $1" >&2; exit 64 ;;
esac
printf "TRACON_SANDBOX_%s='%s'\nTRACON_SANDBOX_PIN='%s'\nTRACON_SANDBOX_SERVER_NAME='%s'\n" "$1" "$2" "$3" "$4" > /run/tracon/bootstrap.new
if [ "$1" = RELAY_URL ]; then
  proxy="${HTTPS_PROXY:-${https_proxy:-}}"
  no_proxy_list="${NO_PROXY:-${no_proxy:-}}"
  case "$proxy" in
    *[!A-Za-z0-9.:/@_%-]*) ;;
    ?*) printf "TRACON_SANDBOX_PROXY='%s'\n" "$proxy" >> /run/tracon/bootstrap.new ;;
  esac
  case "$no_proxy_list" in
    *[!A-Za-z0-9.,:*_\[\]-]*) ;;
    ?*) printf "TRACON_SANDBOX_NO_PROXY='%s'\n" "$no_proxy_list" >> /run/tracon/bootstrap.new ;;
  esac
fi
mv -f /run/tracon/bootstrap.new /run/tracon/bootstrap
if [ ! -x /etc/services.d/tracon-sandbox/run ]; then
  printf '%s' "$7" > /run/tracon/run
  chmod 0700 /run/tracon/run
  setsid -f /run/tracon/run </dev/null >>/run/tracon/log 2>&1
fi
"#;

/// How the sandbox dials the node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dial<'a> {
    /// `ip:port`, raw TLS: the only address the sandbox's egress allows.
    Addr(&'a str),
    /// The hub relay's WebSocket URL, through the runtime's proxy: the
    /// hub's host is the only name the sandbox's egress allows.
    RelayUrl(&'a str),
}

impl<'a> Dial<'a> {
    /// The bootstrap variable it goes in, after `TRACON_SANDBOX_`.
    fn variable(&self) -> &'static str {
        match self {
            Dial::Addr(_) => "ADDR",
            Dial::RelayUrl(_) => "RELAY_URL",
        }
    }

    pub fn value(&self) -> &'a str {
        match self {
            Dial::Addr(v) | Dial::RelayUrl(v) => v,
        }
    }
}

/// What one run's sandbox is told.
#[derive(Clone, PartialEq, Eq)]
pub struct Delivery<'a> {
    pub dial: Dial<'a>,
    pub pin: &'a str,
    /// What the sandbox puts in SNI: the hub relay's name for this node, or
    /// the node's own.
    pub server_name: &'a str,
    pub token: &'a str,
    /// The name the harness's configuration reaches the gateway by, which
    /// the tunnels serve on loopback.
    pub gateway_host: &'a str,
}

impl std::fmt::Debug for Delivery<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Delivery")
            .field("dial", &self.dial)
            .field("pin", &self.pin)
            .field("server_name", &self.server_name)
            .field("token", &"<redacted>")
            .field("gateway_host", &self.gateway_host)
            .finish()
    }
}

/// Characters a value may hold to be written inside single quotes in a file
/// a shell sources, and on an `/etc/hosts` line.
fn plain(what: &str, value: &str, extra: &[char]) -> Result<(), String> {
    if value.is_empty() || value.len() > 253 {
        return Err(format!("{what} is empty or too long"));
    }
    match value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || extra.contains(&c))
    {
        true => Ok(()),
        false => Err(format!("{what} {value:?} has a character it may not")),
    }
}

impl Delivery<'_> {
    pub fn validate(&self) -> Result<(), String> {
        match self.dial {
            Dial::Addr(addr) => plain("the node address", addr, &['.', ':', '[', ']'])?,
            Dial::RelayUrl(url) => {
                plain("the relay URL", url, &['.', ':', '/', '-', '_', '[', ']'])?;
                if !(url.starts_with("wss://") || url.starts_with("ws://")) {
                    return Err(format!("the relay URL {url:?} is not ws:// or wss://"));
                }
            }
        }
        plain("the certificate pin", self.pin, &[':'])?;
        plain("the server name", self.server_name, &['.', '-'])?;
        plain("the token", self.token, &['-', '_'])?;
        plain("the gateway name", self.gateway_host, &['.', '-', '_'])?;
        Ok(())
    }

    /// The exec's argv.
    pub fn argv(&self) -> Result<Vec<String>, String> {
        self.validate()?;
        Ok(vec![
            "/bin/sh".into(),
            "-c".into(),
            DELIVER.into(),
            "tracon-bootstrap".into(),
            self.dial.variable().into(),
            self.dial.value().into(),
            self.pin.into(),
            self.server_name.into(),
            self.token.into(),
            self.gateway_host.into(),
            RUN_SCRIPT.into(),
        ])
    }

    /// Read back what [`Delivery::argv`] made: for a stand-in that plays the
    /// template's service, and for tests.
    pub fn parse(argv: &[String]) -> Option<Delivery<'_>> {
        match argv {
            [sh, c, script, _, variable, dial, pin, server_name, token, gateway_host, _run]
                if sh == "/bin/sh" && c == "-c" && script == DELIVER =>
            {
                let dial = match variable.as_str() {
                    "ADDR" => Dial::Addr(dial),
                    "RELAY_URL" => Dial::RelayUrl(dial),
                    _ => return None,
                };
                Some(Delivery {
                    dial,
                    pin,
                    server_name,
                    token,
                    gateway_host,
                })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delivery() -> Delivery<'static> {
        Delivery {
            dial: Dial::Addr("203.0.113.7:443"),
            pin: "sha256:00aa",
            server_name: "0123abcd.relay.example.net",
            token: "AbC-_9",
            gateway_host: "tracon-gw",
        }
    }

    #[test]
    fn the_argv_reads_back_and_never_logs_the_token() {
        for d in [
            delivery(),
            Delivery {
                dial: Dial::RelayUrl("wss://hub.example.net/v0/relay/connect"),
                ..delivery()
            },
        ] {
            let argv = d.argv().unwrap();
            assert_eq!(Delivery::parse(&argv), Some(d.clone()));
            assert!(!format!("{d:?}").contains("AbC-_9"));
        }
    }

    #[test]
    fn a_value_that_could_break_out_of_its_quotes_is_refused() {
        for bad in [
            Delivery {
                server_name: "x'; rm -rf /; '",
                ..delivery()
            },
            Delivery {
                dial: Dial::Addr("1.2.3.4:443\nX=1"),
                ..delivery()
            },
            Delivery {
                dial: Dial::RelayUrl("wss://hub/'$(id)'"),
                ..delivery()
            },
            Delivery {
                dial: Dial::RelayUrl("wss://hub/x?a=b&c=d"),
                ..delivery()
            },
            Delivery {
                dial: Dial::RelayUrl("https://hub/v0/relay/connect"),
                ..delivery()
            },
            Delivery {
                gateway_host: "gw evil",
                ..delivery()
            },
            Delivery {
                token: "",
                ..delivery()
            },
        ] {
            assert!(bad.argv().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_template_image_installs_this_service() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../containers/do-sandbox/run");
        assert_eq!(std::fs::read_to_string(path).unwrap(), RUN_SCRIPT);
    }

    /// Run the delivery against a scratch directory standing in for
    /// `/run/tracon` and `/etc/hosts`, with `env`; the bootstrap it wrote.
    fn delivered(d: &Delivery<'_>, env: &[(&str, &str)]) -> String {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run");
        let hosts = dir.path().join("hosts");
        let mut argv = d.argv().unwrap();
        argv[2] = DELIVER
            .replace("/run/tracon", &run.to_string_lossy())
            .replace("/etc/hosts", &hosts.to_string_lossy())
            // Present and executable: the template's service is installed.
            .replace("/etc/services.d/tracon-sandbox/run", "/bin/sh");
        let out = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .envs(env.iter().copied())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(std::fs::read_to_string(run.join("token")).unwrap(), d.token);
        assert_eq!(
            std::fs::read_to_string(&hosts).unwrap(),
            format!("127.0.0.1 {}\n", d.gateway_host)
        );
        std::fs::read_to_string(run.join("bootstrap")).unwrap()
    }

    #[test]
    fn the_bootstrap_names_the_dial_and_a_websocket_gets_the_proxy() {
        let on_do = [
            ("HTTPS_PROXY", "http://169.254.169.254:3128"),
            (
                "NO_PROXY",
                "169.254.169.254,localhost,127.0.0.1,.vpc-endpoint.internal.digitalocean.com",
            ),
        ];
        assert_eq!(
            delivered(&delivery(), &on_do),
            "TRACON_SANDBOX_ADDR='203.0.113.7:443'\nTRACON_SANDBOX_PIN='sha256:00aa'\n\
             TRACON_SANDBOX_SERVER_NAME='0123abcd.relay.example.net'\n"
        );
        let ws = Delivery {
            dial: Dial::RelayUrl("wss://hub.example.net/v0/relay/connect"),
            ..delivery()
        };
        assert_eq!(
            delivered(&ws, &on_do),
            "TRACON_SANDBOX_RELAY_URL='wss://hub.example.net/v0/relay/connect'\n\
             TRACON_SANDBOX_PIN='sha256:00aa'\n\
             TRACON_SANDBOX_SERVER_NAME='0123abcd.relay.example.net'\n\
             TRACON_SANDBOX_PROXY='http://169.254.169.254:3128'\n\
             TRACON_SANDBOX_NO_PROXY='169.254.169.254,localhost,127.0.0.1,.vpc-endpoint.internal.digitalocean.com'\n"
        );
        // Nothing to copy; and a value that could leave its quotes is not.
        let bare = delivered(&ws, &[]);
        assert!(!bare.contains("PROXY"), "{bare}");
        let hostile = delivered(
            &ws,
            &[("https_proxy", "http://p:1'\nX='1"), ("no_proxy", "a b")],
        );
        assert!(!hostile.contains("PROXY"), "{hostile}");
        let lower = delivered(&ws, &[("https_proxy", "http://p:3128")]);
        assert!(
            lower.contains("TRACON_SANDBOX_PROXY='http://p:3128'\n"),
            "{lower}"
        );
    }

    #[test]
    fn the_scripts_parse_as_shell() {
        for script in [DELIVER, RUN_SCRIPT] {
            let status = std::process::Command::new("sh")
                .args(["-n", "-c", script])
                .status()
                .unwrap();
            assert!(status.success(), "{script}");
        }
    }
}
