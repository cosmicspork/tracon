//! The node's endpoints a sandbox reaches through tunnels over its protocol
//! connection, when the connection is its only route: the harness listener
//! (the MCP gateway and the model gateway), the allowlist proxy and the
//! per-client egress proxy. Each listens on the sandbox's loopback at the
//! port the gateway container would have offered it on, so a harness
//! addresses them exactly as it always has (`http://<gateway>:<port>`, the
//! gateway's name mapped to loopback by the provider).
//!
//! Nothing here knows the provider: any sandbox reached over the protocol
//! gets the same tunnels. What decides who may use an endpoint is the
//! endpoint, as before: the session's gateway token, the grant's
//! credentials. A tunnel only carries bytes.

use std::net::Ipv4Addr;

use tracon_sandbox::node::{Endpoint, SandboxConn, Tunnel};
use tracon_sandbox::protocol::ListenAddr;

use super::RunnerError;
use crate::config::{Config, HarnessListen};

/// One listener inside the sandbox, and where its connections go on the
/// node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelSpec {
    pub name: String,
    pub listen: ListenAddr,
    pub endpoint: Endpoint,
}

impl TunnelSpec {
    /// Listening on the sandbox's loopback at `port`.
    pub fn loopback(name: &str, port: u16, endpoint: Endpoint) -> Self {
        TunnelSpec {
            name: name.into(),
            listen: ListenAddr::Tcp(format!("{}:{port}", Ipv4Addr::LOCALHOST)),
            endpoint,
        }
    }
}

fn endpoint(listen: &HarnessListen) -> Endpoint {
    match listen {
        HarnessListen::Tcp(addr) => Endpoint::Tcp(addr.to_string()),
        HarnessListen::Unix(path) => Endpoint::Unix(path.clone()),
    }
}

/// The tunnels that stand in for the gateway container: its forward to the
/// harness listener (`forward_port`), its allowlist proxy (`proxy_port`),
/// served by the node itself, and its forward to the per-client egress
/// proxy (`qa_proxy_port`, when configured).
pub fn gateway_tunnels(cfg: &Config) -> Vec<TunnelSpec> {
    let mut tunnels = vec![
        TunnelSpec::loopback(
            "gateway",
            cfg.gateway.forward_port,
            endpoint(&cfg.gateway.harness_listen),
        ),
        TunnelSpec::loopback(
            "proxy",
            cfg.gateway.proxy_port,
            Endpoint::Unix(cfg.allow_proxy_socket()),
        ),
    ];
    if cfg.gateway.qa_proxy_port != 0 {
        tunnels.push(TunnelSpec::loopback(
            "egress",
            cfg.gateway.qa_proxy_port,
            endpoint(&cfg.egress_listen()),
        ));
    }
    tunnels
}

/// The tunnels a Kubernetes sandbox gets in place of its routes to the node
/// pod: the harness listener at the forward port, the node's own allowlist
/// proxy at `proxy_port`, and its per-client egress proxy at `egress_port`,
/// each where a harness pod reached it before (`http://<gateway_host>:<port>`,
/// the gateway's name now the sandbox's loopback). They lead to the node's
/// listeners on its own loopback, which serve on every address of its pod.
pub fn kube_tunnels(cfg: &Config) -> Vec<TunnelSpec> {
    let local = |port: u16| Endpoint::Tcp(format!("{}:{port}", Ipv4Addr::LOCALHOST));
    let forward = match &cfg.gateway.harness_listen {
        HarnessListen::Tcp(addr) => addr.port(),
        HarnessListen::Unix(_) => cfg.gateway.forward_port,
    };
    let mut tunnels = vec![
        TunnelSpec::loopback("gateway", forward, endpoint(&cfg.gateway.harness_listen)),
        TunnelSpec::loopback(
            "proxy",
            cfg.gateway.proxy_port,
            local(cfg.gateway.proxy_port),
        ),
        TunnelSpec::loopback(
            "egress",
            cfg.gateway.egress_port,
            local(cfg.gateway.egress_port),
        ),
    ];
    // An unspecified listener address is the node's every address, its
    // loopback among them; dialling the unspecified address is not portable.
    for tunnel in &mut tunnels {
        if let Endpoint::Tcp(addr) = &tunnel.endpoint {
            if let Ok(addr) = addr.parse::<std::net::SocketAddr>() {
                if addr.ip().is_unspecified() {
                    tunnel.endpoint = local(addr.port());
                }
            }
        }
    }
    tunnels
}

/// Open every tunnel on `conn`, before anything is started in the sandbox
/// that might dial one. All or nothing: on a failure the ones already open
/// are dropped, and the sandbox's teardown closes their listeners.
pub async fn open(conn: &SandboxConn, specs: &[TunnelSpec]) -> Result<Vec<Tunnel>, RunnerError> {
    let mut open = Vec::with_capacity(specs.len());
    for spec in specs {
        let tunnel = conn
            .open_tunnel(
                spec.name.clone(),
                spec.listen.clone(),
                spec.endpoint.clone(),
            )
            .await
            .map_err(|e| RunnerError::Other(format!("open tunnel {}: {e}", spec.name)))?;
        open.push(tunnel);
    }
    Ok(open)
}

/// Take back every tunnel of a sandbox an earlier node process opened them
/// in: its listeners are still there, so only the node's side starts again.
pub fn adopt(conn: &SandboxConn, specs: &[TunnelSpec]) -> Vec<Tunnel> {
    specs
        .iter()
        .map(|spec| {
            conn.adopt_tunnel(
                spec.name.clone(),
                spec.listen.clone(),
                spec.endpoint.clone(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gateway_tunnels_listen_where_the_gateway_did_and_lead_to_the_nodes_listeners() {
        let mut cfg = Config::default();
        cfg.gateway.harness_listen = HarnessListen::Unix("/run/t/harness.sock".into());
        let tunnels = gateway_tunnels(&cfg);
        assert_eq!(
            tunnels,
            vec![
                TunnelSpec {
                    name: "gateway".into(),
                    listen: ListenAddr::Tcp(format!("127.0.0.1:{}", cfg.gateway.forward_port)),
                    endpoint: Endpoint::Unix("/run/t/harness.sock".into()),
                },
                TunnelSpec {
                    name: "proxy".into(),
                    listen: ListenAddr::Tcp(format!("127.0.0.1:{}", cfg.gateway.proxy_port)),
                    endpoint: Endpoint::Unix("/run/t/allow.sock".into()),
                },
                TunnelSpec {
                    name: "egress".into(),
                    listen: ListenAddr::Tcp(format!("127.0.0.1:{}", cfg.gateway.qa_proxy_port)),
                    endpoint: Endpoint::Unix("/run/t/egress.sock".into()),
                },
            ]
        );

        // No per-client proxy configured, no tunnel to it; a TCP harness
        // listener is reached at its address.
        cfg.gateway.qa_proxy_port = 0;
        cfg.gateway.harness_listen = HarnessListen::Tcp("127.0.0.1:7499".parse().unwrap());
        let tunnels = gateway_tunnels(&cfg);
        assert_eq!(tunnels.len(), 2);
        assert_eq!(tunnels[0].endpoint, Endpoint::Tcp("127.0.0.1:7499".into()));
    }

    /// A pod-hosted node's listeners face its pod network; a sandbox reaches
    /// each through a tunnel on its own loopback, at the port it reached the
    /// node pod on before, and the tunnel dials the node's loopback.
    #[test]
    fn the_kube_tunnels_stand_in_for_the_node_pods_ports() {
        let mut cfg = Config::default();
        cfg.gateway.harness_listen = HarnessListen::Tcp("0.0.0.0:7421".parse().unwrap());
        let tunnels = kube_tunnels(&cfg);
        let shape: Vec<(&str, ListenAddr, Endpoint)> = tunnels
            .iter()
            .map(|t| (t.name.as_str(), t.listen.clone(), t.endpoint.clone()))
            .collect();
        let local = |port: u16| ListenAddr::Tcp(format!("127.0.0.1:{port}"));
        let node = |port: u16| Endpoint::Tcp(format!("127.0.0.1:{port}"));
        assert_eq!(
            shape,
            vec![
                ("gateway", local(7421), node(7421)),
                ("proxy", local(8888), node(8888)),
                ("egress", local(7424), node(7424)),
            ]
        );
    }
}
