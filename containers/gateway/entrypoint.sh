#!/bin/sh
# Gateway: allowlist HTTPS CONNECT proxy for the harness, plus a forward from the
# internal network to the node. TRACON_UPSTREAM is a socat address such as
# UNIX-CONNECT:/run/tracon/node.sock or TCP:host.containers.internal:7421.
#
# TRACON_QA_PROXY_PORT, when set, starts a second, independent CONNECT proxy
# filtered by qa_allow.txt instead of allow.txt: a QA browser run's egress
# and an ordinary harness session's egress are two separate filters, so
# rewriting one (the node does, per QA run) never widens the other.
set -eu
: "${TRACON_UPSTREAM:?TRACON_UPSTREAM is required}"
: "${TRACON_LISTEN_IP:=10.89.0.2}"
test -r /etc/tinyproxy/allow.txt || { echo "allow.txt missing" >&2; exit 1; }
# Bind the CONNECT proxy to the internal gateway IP only, so it is not offered on
# the default network the gateway also joins. The conf ships a placeholder Listen.
conf=/tmp/tinyproxy.conf
sed "s/^Listen .*/Listen ${TRACON_LISTEN_IP}/" /etc/tinyproxy/tinyproxy.conf > "$conf"
socat "TCP-LISTEN:7421,bind=${TRACON_LISTEN_IP},fork,reuseaddr" "${TRACON_UPSTREAM}" &
if [ -n "${TRACON_QA_PROXY_PORT:-}" ]; then
  test -r /etc/tinyproxy/qa_allow.txt || { echo "qa_allow.txt missing" >&2; exit 1; }
  qa_conf=/tmp/qa-tinyproxy.conf
  sed \
    -e "s/^Listen .*/Listen ${TRACON_LISTEN_IP}/" \
    -e "s/^Port .*/Port ${TRACON_QA_PROXY_PORT}/" \
    -e 's#^PidFile .*#PidFile "/run/tinyproxy-qa.pid"#' \
    -e 's#^Filter .*#Filter "/etc/tinyproxy/qa_allow.txt"#' \
    /etc/tinyproxy/tinyproxy.conf > "$qa_conf"
  tinyproxy -d -c "$qa_conf" &
fi
exec tinyproxy -d -c "$conf"
