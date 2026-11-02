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
/// node's address, the certificate pin, the server name, the token and the
/// gateway's name. The token is written before the bootstrap that names it,
/// so the service never starts without one.
const DELIVER: &str = r#"set -eu
umask 077
mkdir -p /run/tracon
grep -qs "^127.0.0.1 $5\$" /etc/hosts || printf '127.0.0.1 %s\n' "$5" >> /etc/hosts
printf '%s' "$4" > /run/tracon/token.new
mv -f /run/tracon/token.new /run/tracon/token
printf "TRACON_SANDBOX_ADDR='%s'\nTRACON_SANDBOX_PIN='%s'\nTRACON_SANDBOX_SERVER_NAME='%s'\n" "$1" "$2" "$3" > /run/tracon/bootstrap.new
mv -f /run/tracon/bootstrap.new /run/tracon/bootstrap
if [ ! -x /etc/services.d/tracon-sandbox/run ]; then
  printf '%s' "$6" > /run/tracon/run
  chmod 0700 /run/tracon/run
  setsid -f /run/tracon/run </dev/null >>/run/tracon/log 2>&1
fi
"#;

/// What one run's sandbox is told.
#[derive(Clone, PartialEq, Eq)]
pub struct Delivery<'a> {
    /// `ip:port`: the only address the sandbox's egress allows.
    pub addr: &'a str,
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
            .field("addr", &self.addr)
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
        plain("the node address", self.addr, &['.', ':', '[', ']'])?;
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
            self.addr.into(),
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
            [sh, c, script, _, addr, pin, server_name, token, gateway_host, _run]
                if sh == "/bin/sh" && c == "-c" && script == DELIVER =>
            {
                Some(Delivery {
                    addr,
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
            addr: "203.0.113.7:443",
            pin: "sha256:00aa",
            server_name: "0123abcd.relay.example.net",
            token: "AbC-_9",
            gateway_host: "tracon-gw",
        }
    }

    #[test]
    fn the_argv_reads_back_and_never_logs_the_token() {
        let d = delivery();
        let argv = d.argv().unwrap();
        assert_eq!(Delivery::parse(&argv), Some(d.clone()));
        assert!(!format!("{d:?}").contains("AbC-_9"));
    }

    #[test]
    fn a_value_that_could_break_out_of_its_quotes_is_refused() {
        for bad in [
            Delivery {
                server_name: "x'; rm -rf /; '",
                ..delivery()
            },
            Delivery {
                addr: "1.2.3.4:443\nX=1",
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
