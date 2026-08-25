#!/bin/sh
# Gateway: allowlist HTTPS CONNECT proxy for the harness, plus a forward from the
# internal network to the node. TRACON_UPSTREAM is a socat address such as
# UNIX-CONNECT:/run/tracon/node.sock or TCP:host.containers.internal:7421.
set -eu
: "${TRACON_UPSTREAM:?TRACON_UPSTREAM is required}"
: "${TRACON_LISTEN_IP:=10.89.0.2}"
test -r /etc/tinyproxy/allow.txt || { echo "allow.txt missing" >&2; exit 1; }
# Bind the CONNECT proxy to the internal gateway IP only, so it is not offered on
# the default network the gateway also joins. The conf ships a placeholder Listen.
conf=/tmp/tinyproxy.conf
sed "s/^Listen .*/Listen ${TRACON_LISTEN_IP}/" /etc/tinyproxy/tinyproxy.conf > "$conf"
socat "TCP-LISTEN:7421,bind=${TRACON_LISTEN_IP},fork,reuseaddr" "${TRACON_UPSTREAM}" &
exec tinyproxy -d -c "$conf"
