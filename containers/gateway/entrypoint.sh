#!/bin/sh
# Gateway: allowlist HTTPS CONNECT proxy for the harness, plus forwards from the
# internal network to the node. TRACON_UPSTREAM is a socat address such as
# UNIX-CONNECT:/run/tracon/node.sock or TCP:host.containers.internal:7421.
#
# TRACON_EGRESS_UPSTREAM, when set, is forwarded the same way from
# TRACON_EGRESS_PORT: the node's own per-client egress proxy, where a QA
# browser run, a dependency preparation and a session each present their own
# credentials and are filtered by their own grant. Nothing here decides what
# any of them may reach; this only carries the connection to the node.
set -eu
: "${TRACON_UPSTREAM:?TRACON_UPSTREAM is required}"
: "${TRACON_LISTEN_IP:=10.89.0.2}"
test -r /etc/tinyproxy/allow.txt || { echo "allow.txt missing" >&2; exit 1; }
# Bind the CONNECT proxy to the internal gateway IP only, so it is not offered on
# the default network the gateway also joins. The conf ships a placeholder Listen.
conf=/tmp/tinyproxy.conf
sed "s/^Listen .*/Listen ${TRACON_LISTEN_IP}/" /etc/tinyproxy/tinyproxy.conf > "$conf"
socat "TCP-LISTEN:7421,bind=${TRACON_LISTEN_IP},fork,reuseaddr" "${TRACON_UPSTREAM}" &
if [ -n "${TRACON_EGRESS_UPSTREAM:-}" ]; then
  : "${TRACON_EGRESS_PORT:?TRACON_EGRESS_PORT is required with TRACON_EGRESS_UPSTREAM}"
  socat "TCP-LISTEN:${TRACON_EGRESS_PORT},bind=${TRACON_LISTEN_IP},fork,reuseaddr" "${TRACON_EGRESS_UPSTREAM}" &
fi
exec tinyproxy -d -c "$conf"
