"""Fake Anthropic provider + fake MCP server for the OpenCode v1 spike.

Provider: logs every request to provider.log (path, headers, tool names offered,
messages tail). Scripted replies keyed on turn count:
  turn 1 -> tool_use of the MCP tool `tracon_ping` (proves MCP tools are offered/called)
  turn 2 -> tool_use of built-in `bash` (proves a permission ask is raised)
  turn 3 -> end_turn text
MCP: streamable-HTTP JSON-RPC; offers one tool `tracon_ping`; logs calls to mcp.log.
"""
import json, sys, threading, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOG = sys.argv[1] if len(sys.argv) > 1 else "."
turns = {"n": 0}
lock = threading.Lock()


def log(name, obj):
    with open(f"{LOG}/{name}.log", "a") as f:
        f.write(json.dumps(obj) + "\n")


def sse(events):
    out = []
    for ev, data in events:
        out.append(f"event: {ev}\ndata: {json.dumps(data)}\n\n")
    return "".join(out).encode()


def anthropic_reply(turn, tools):
    names = [t.get("name") for t in tools]
    if turn == 1 and any(n and n.endswith("tracon_ping") for n in names):
        name = next(n for n in names if n.endswith("tracon_ping"))
        block = {"type": "tool_use", "id": "toolu_1", "name": name}
        args = {"note": "hello from the model"}
    elif turn <= 2 and "bash" in names:
        block = {"type": "tool_use", "id": f"toolu_{turn}", "name": "bash"}
        args = {"command": "echo spike-bash-ran", "description": "spike"}
    else:
        return sse([
            ("message_start", {"type": "message_start", "message": {"id": f"msg_{turn}", "type": "message", "role": "assistant", "model": "claude-x", "content": [], "stop_reason": None, "usage": {"input_tokens": 10, "output_tokens": 1}}}),
            ("content_block_start", {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            ("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "spike done"}}),
            ("content_block_stop", {"type": "content_block_stop", "index": 0}),
            ("message_delta", {"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": None}, "usage": {"output_tokens": 3}}),
            ("message_stop", {"type": "message_stop"}),
        ])
    return sse([
        ("message_start", {"type": "message_start", "message": {"id": f"msg_{turn}", "type": "message", "role": "assistant", "model": "claude-x", "content": [], "stop_reason": None, "usage": {"input_tokens": 10, "output_tokens": 1}}}),
        ("content_block_start", {"type": "content_block_start", "index": 0, "content_block": {**block, "input": {}}}),
        ("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": json.dumps(args)}}),
        ("content_block_stop", {"type": "content_block_stop", "index": 0}),
        ("message_delta", {"type": "message_delta", "delta": {"stop_reason": "tool_use", "stop_sequence": None}, "usage": {"output_tokens": 5}}),
        ("message_stop", {"type": "message_stop"}),
    ])


class Provider(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def do_POST(self):
        n = int(self.headers.get("content-length", 0))
        body = json.loads(self.rfile.read(n) or b"{}")
        with lock:
            turns["n"] += 1
            turn = turns["n"]
        tools = body.get("tools", [])
        log("provider", {
            "turn": turn, "path": self.path,
            "headers": {k: v for k, v in self.headers.items()},
            "tools": [t.get("name") for t in tools],
            "last_message": body.get("messages", [])[-1] if body.get("messages") else None,
            "system_len": len(json.dumps(body.get("system", ""))),
        })
        if not self.path.endswith("/messages"):
            self.send_response(404); self.send_header("content-length", "0"); self.end_headers(); return
        out = anthropic_reply(turn, tools)
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)

    def do_GET(self):
        log("provider", {"path": self.path, "method": "GET"})
        self.send_response(404); self.send_header("content-length", "0"); self.end_headers()


class Mcp(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def reply(self, code, obj=None, extra=None):
        data = json.dumps(obj).encode() if obj is not None else b""
        self.send_response(code)
        if obj is not None:
            self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.send_header("mcp-session-id", "spike-mcp")
        for k, v in (extra or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        # No server-initiated stream offered.
        self.reply(405)

    def do_DELETE(self):
        self.reply(200, {})

    def do_POST(self):
        n = int(self.headers.get("content-length", 0))
        msg = json.loads(self.rfile.read(n) or b"{}")
        log("mcp", {"headers": {k: v for k, v in self.headers.items()}, "msg": msg})
        if isinstance(msg, list):
            msg = msg[0]
        method = msg.get("method")
        rid = msg.get("id")
        if method == "initialize":
            self.reply(200, {"jsonrpc": "2.0", "id": rid, "result": {
                "protocolVersion": msg["params"].get("protocolVersion", "2025-03-26"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "tracon-spike", "version": "0"}}})
        elif method == "tools/list":
            self.reply(200, {"jsonrpc": "2.0", "id": rid, "result": {"tools": [{
                "name": "tracon_ping",
                "description": "Spike: records that the model called a tracon MCP tool.",
                "inputSchema": {"type": "object", "properties": {"note": {"type": "string"}}}}]}})
        elif method == "tools/call":
            self.reply(200, {"jsonrpc": "2.0", "id": rid, "result": {
                "content": [{"type": "text", "text": "pong: " + json.dumps(msg["params"].get("arguments"))}]}})
        elif rid is None:
            self.reply(202)
        else:
            self.reply(200, {"jsonrpc": "2.0", "id": rid, "error": {"code": -32601, "message": f"no {method}"}})


if __name__ == "__main__":
    pport, mport = int(sys.argv[2]), int(sys.argv[3])
    p = ThreadingHTTPServer(("127.0.0.1", pport), Provider)
    m = ThreadingHTTPServer(("127.0.0.1", mport), Mcp)
    threading.Thread(target=p.serve_forever, daemon=True).start()
    threading.Thread(target=m.serve_forever, daemon=True).start()
    print("fakes up", flush=True)
    while True:
        time.sleep(3600)
