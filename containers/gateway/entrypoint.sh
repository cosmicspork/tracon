#!/bin/sh
# Gateway: allowlist HTTPS CONNECT proxy for the harness, plus a forward from the
# internal network to the node. TRACON_UPSTREAM is a socat address such as
# UNIX-CONNECT:/run/tracon/node.sock or TCP:host.containers.internal:7421.
set -eu
: "${TRACON_UPSTREAM:?TRACON_UPSTREAM is required}"
: "${TRACON_LISTEN_IP:=10.89.0.2}"
test -r /etc/tinyproxy/allow.txt || { echo "allow.txt missing" >&2; exit 1; }
socat "TCP-LISTEN:7421,bind=${TRACON_LISTEN_IP},fork,reuseaddr" "${TRACON_UPSTREAM}" &
exec tinyproxy -d -c /etc/tinyproxy/tinyproxy.conf
