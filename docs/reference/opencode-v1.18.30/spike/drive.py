"""Drive the pinned opencode binary over the v1 or v2 routes the way an adapter would.

Finding 23 in ../README.md is this script's output. Rerun it on an upgrade:

    mkdir spike && c=$(podman create localhost/tracon-harness-opencode)
    podman cp $c:/usr/local/bin/opencode spike/opencode
    podman cp $c:/opt/tracon/seed spike/seed && podman rm $c
    cp docs/reference/opencode-v1.18.30/spike/*.py spike/
    python3 spike/drive.py spike --route v1
    python3 spike/drive.py spike --route v2
    SPIKE_DROP=api python3 spike/drive.py spike --route v1     # or SPIKE_DROP=baseURL

Everything lands under <spike_dir>/run-<route>/: the rendered config, server.out,
provider.log (every model call: host, headers, tools offered), mcp.log, events.jsonl
and result.json. The provider log's `tools` is the finding.
"""
import json, os, shutil, signal, socket, subprocess, sys, threading, time, urllib.request, base64

SPIKE = os.path.abspath(sys.argv[1])
ROUTE = "v1"
if "--route" in sys.argv:
    ROUTE = sys.argv[sys.argv.index("--route") + 1]
RUN = f"{SPIKE}/run-{ROUTE}" + (("-drop-" + os.environ["SPIKE_DROP"]) if os.environ.get("SPIKE_DROP") else "")
shutil.rmtree(RUN, ignore_errors=True)
os.makedirs(RUN)
for d in ["home", "data", "cache", "state", "xdg/opencode", "work"]:
    os.makedirs(f"{RUN}/{d}", exist_ok=True)


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


PPORT, MPORT, OPORT = free_port(), free_port(), free_port()
GW = f"http://127.0.0.1:{PPORT}/model/anthropic/v1"
MCP = f"http://127.0.0.1:{MPORT}/mcp/s1"
PASSWORD = "spike-password"
TOKEN = "spike-placeholder-token"

model = {
    "id": "claude-x", "name": "claude-x", "reasoning": False, "attachment": False, "tool_call": True,
    "limit": {"context": 200000, "output": 64000},
}
config = {
    "$schema": "https://opencode.ai/config.json",
    "provider": {"anthropic": {
        "npm": "@ai-sdk/anthropic", "name": "anthropic (tracon)",
        "api": GW, "options": {"baseURL": GW, "apiKey": TOKEN},
        "models": {"claude-x": model}}},
    "disabled_providers": ["opencode", "openai", "openai-codex", "google", "github-copilot"],
    "enabled_providers": ["anthropic"],
    "permission": {"*": "ask"},
    "share": "disabled", "autoshare": False, "autoupdate": False,
    "experimental": {"mcp_timeout": 20000},
    "mcp": {"tracon": {"type": "remote", "url": MCP, "headers": {"Authorization": f"Bearer {TOKEN}"}, "oauth": False, "timeout": 20000}},
}
cat_model = {**model, "release_date": "", "temperature": False, "cost": {"input": 0, "output": 0},
             "modalities": {"input": ["text"], "output": ["text"]}, "status": "active",
             "provider": {"npm": "@ai-sdk/anthropic", "api": GW}}
catalogue = {"anthropic": {"id": "anthropic", "name": "anthropic (tracon)", "npm": "@ai-sdk/anthropic",
                           "api": GW, "env": ["TRACON_PROVIDER_KEY_ANTHROPIC"], "models": {"claude-x": cat_model}}}
DROP = os.environ.get("SPIKE_DROP")
if DROP == "api":
    del config["provider"]["anthropic"]["api"]
    for m in catalogue["anthropic"]["models"].values(): m["provider"].pop("api")
    catalogue["anthropic"].pop("api")
if DROP == "baseURL":
    del config["provider"]["anthropic"]["options"]["baseURL"]
json.dump(config, open(f"{RUN}/opencode.json", "w"), indent=1)
json.dump(config, open(f"{RUN}/xdg/opencode/opencode.json", "w"), indent=1)
json.dump(catalogue, open(f"{RUN}/models.json", "w"), indent=1)
open(f"{RUN}/xdg/opencode/AGENTS.md", "w").write("# spike orientation\nYou are in the tracon v1 spike.\n")
# plugin seed so the install never reaches the registry
if os.path.isdir(f"{SPIKE}/seed/config"):
    shutil.copytree(f"{SPIKE}/seed/config", f"{RUN}/xdg/opencode", dirs_exist_ok=True)
subprocess.run(["git", "init", "-q", f"{RUN}/work"], check=True)
open(f"{RUN}/work/README.md", "w").write("spike\n")

fakes = subprocess.Popen([sys.executable, f"{SPIKE}/fakes.py", RUN, str(PPORT), str(MPORT)],
                         stdout=open(f"{RUN}/fakes.out", "w"), stderr=subprocess.STDOUT)
env = {
    "PATH": os.environ["PATH"], "HOME": f"{RUN}/home",
    "XDG_CONFIG_HOME": f"{RUN}/xdg", "XDG_DATA_HOME": f"{RUN}/data", "XDG_CACHE_HOME": f"{RUN}/cache",
    "XDG_STATE_HOME": f"{RUN}/state", "OPENCODE_DB": f"{RUN}/state/opencode.db",
    "OPENCODE_CONFIG": f"{RUN}/opencode.json", "OPENCODE_MODELS_PATH": f"{RUN}/models.json",
    "OPENCODE_DISABLE_MODELS_FETCH": "true", "OPENCODE_DISABLE_PROJECT_CONFIG": "true", "OPENCODE_PURE": "1",
    "OPENCODE_DISABLE_DEFAULT_PLUGINS": "true", "OPENCODE_DISABLE_EXTERNAL_SKILLS": "true",
    "OPENCODE_DISABLE_CLAUDE_CODE": "true", "OPENCODE_DISABLE_LSP_DOWNLOAD": "true",
    "OPENCODE_DISABLE_AUTOUPDATE": "true", "OPENCODE_DISABLE_SHARE": "true",
    "OPENCODE_AUTH_CONTENT": "{}", "OPENCODE_SERVER_USERNAME": "opencode", "OPENCODE_SERVER_PASSWORD": PASSWORD,
    "TRACON_PROVIDER_KEY_ANTHROPIC": TOKEN,
    # a proxy nothing should use; if the harness reaches for the world we see it fail, not succeed
    "HTTP_PROXY": "http://127.0.0.1:9", "HTTPS_PROXY": "http://127.0.0.1:9", "NO_PROXY": "127.0.0.1,localhost",
}
AUTH = "Basic " + base64.b64encode(f"opencode:{PASSWORD}".encode()).decode()
BASE = f"http://127.0.0.1:{OPORT}"


def start_server():
    return subprocess.Popen([f"{SPIKE}/opencode", "serve", "--hostname", "127.0.0.1", "--port", str(OPORT), "--mdns=false"],
                            cwd=f"{RUN}/work", env=env,
                            stdout=open(f"{RUN}/server.out", "a"), stderr=subprocess.STDOUT)


def req(method, path, body=None, timeout=30):
    data = json.dumps(body).encode() if body is not None else None
    r = urllib.request.Request(BASE + path, data=data, method=method,
                               headers={"Authorization": AUTH, "content-type": "application/json"})
    try:
        with urllib.request.urlopen(r, timeout=timeout) as resp:
            raw = resp.read()
            return resp.status, (json.loads(raw) if raw else None)
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(errors="replace")


def wait_up():
    for _ in range(300):
        try:
            s, _ = req("GET", "/global/health")
            if s == 200:
                return
        except Exception:
            pass
        time.sleep(0.2)
    raise SystemExit("server never came up; see server.out")


events = []
stop_events = threading.Event()


def event_reader(path):
    r = urllib.request.Request(BASE + path, headers={"Authorization": AUTH, "accept": "text/event-stream"})
    try:
        with urllib.request.urlopen(r, timeout=600) as resp:
            buf = b""
            while not stop_events.is_set():
                line = resp.readline()
                if not line:
                    break
                if line.startswith(b"data:"):
                    try:
                        ev = json.loads(line[5:].strip())
                    except Exception:
                        continue
                    events.append(ev)
                    with open(f"{RUN}/events.jsonl", "a") as f:
                        f.write(json.dumps(ev) + "\n")
    except Exception as e:
        events.append({"type": "_reader_error", "error": str(e)})


result = {"route": ROUTE, "steps": []}


def step(name, **kw):
    result["steps"].append({"step": name, **kw}); print(name, json.dumps(kw)[:300], flush=True)


server = start_server()
try:
    wait_up()
    q = f"?directory={RUN}/work"
    s, body = req("GET", "/config/providers" + q)
    step("providers", status=s, ids=[p.get("id") for p in (body or {}).get("providers", [])] if isinstance(body, dict) else body)
    s, body = req("GET", "/mcp" + q)
    step("mcp_status", status=s, body=body)
    s, body = req("GET", "/agent" + q)
    step("agents", status=s, names=[a.get("name") for a in body] if isinstance(body, list) else body)

    threading.Thread(target=event_reader, args=["/event" + q], daemon=True).start()
    time.sleep(0.5)
    if ROUTE == "v1":
        s, sess = req("POST", "/session" + q, {"title": "spike"})
    else:
        s, sess = req("POST", "/api/session", {"model": {"providerID": "anthropic", "id": "claude-x"},
                                                "location": {"directory": f"{RUN}/work"}})
        sess = sess.get("data", sess) if isinstance(sess, dict) else sess
    step("create_session", status=s, id=sess.get("id") if isinstance(sess, dict) else sess)
    sid = sess["id"]

    prompt = {"model": {"providerID": "anthropic", "modelID": "claude-x"},
              "parts": [{"type": "text", "text": "call tracon_ping, then run bash"}]}
    if ROUTE != "v1":
        prompt = {"prompt": {"text": "call tracon_ping, then run bash"}}
    path = f"/session/{sid}/prompt_async" if ROUTE == "v1" else f"/api/session/{sid}/prompt"
    try:
        s, body = req("POST", path + q, prompt, timeout=10)
        step("prompt_sent", path=path, status=s, body=str(body)[:200])
    except Exception as e:
        step("prompt_sent", path=path, error=str(e))

    # answer permission asks as they appear, up to a deadline
    answered = set()
    deadline = time.time() + 60
    while time.time() < deadline:
        for ev in list(events):
            t = ev.get("type", "")
            props = ev.get("properties", {})
            if t in ("permission.updated", "permission.asked", "permission.v2.asked"):
                pid = props.get("id") or props.get("requestID")
                if pid and pid not in answered:
                    answered.add(pid)
                    tried = []
                    for path_, body_ in [(f"/session/{sid}/permissions/{pid}", {"response": "once"}),
                                         (f"/permission/{pid}/reply", {"reply": "once"}),
                                         (f"/session/{sid}/permission/{pid}/reply", {"reply": "once"})]:
                        try:
                            s, r = req("POST", path_ + q, body_, timeout=10)
                        except Exception as e:
                            s, r = "exc", str(e)
                        tried.append((path_, s, str(r)[:120]))
                        if s == 200:
                            break
                    r = tried
                    step("permission_reply", event=t, id=pid, status=s, resp=str(r)[:200],
                         permission=props.get("permission") or props.get("type") or props.get("pattern"))
        if any(e.get("type") == "session.idle" or (e.get("type") == "session.status" and e.get("properties", {}).get("status", {}).get("type") == "idle") for e in events) and len(answered) >= 1 and time.time() > deadline - 50:
            pass
        idle = [e for e in events if e.get("type") in ("session.idle",) or (e.get("type") == "session.status" and (e.get("properties", {}).get("status") or {}).get("type") == "idle")]
        if idle and answered:
            # give the loop a moment to see any further asks
            time.sleep(2)
            if not [e for e in events if e.get("type") in ("permission.updated", "permission.asked") and (e["properties"].get("id") or e["properties"].get("requestID")) not in answered]:
                break
        time.sleep(0.3)

    s, msgs = req("GET", f"/session/{sid}/message" + q)
    parts = []
    for m in msgs if isinstance(msgs, list) else []:
        for p in m.get("parts", []):
            parts.append({"role": m.get("info", {}).get("role"), "type": p.get("type"), "tool": p.get("tool"),
                          "state": (p.get("state") or {}).get("status"), "text": (p.get("text") or "")[:80],
                          "output": ((p.get("state") or {}).get("output") or "")[:80]})
    step("messages_before_restart", status=s, count=len(msgs) if isinstance(msgs, list) else msgs, parts=parts)

    # reconnect story: restart the server, ask for the same session again
    stop_events.set()
    server.terminate(); server.wait(timeout=15)
    server = start_server(); wait_up()
    s, msgs2 = req("GET", f"/session/{sid}/message" + q)
    s2, sess2 = req("GET", f"/session/{sid}" + q)
    step("after_restart", session_status=s2, messages_status=s,
         count=len(msgs2) if isinstance(msgs2, list) else msgs2,
         same_count=(isinstance(msgs, list) and isinstance(msgs2, list) and len(msgs) == len(msgs2)))
    s, sessions = req("GET", "/session" + q)
    step("session_list_after_restart", status=s, ids=[x.get("id") for x in sessions] if isinstance(sessions, list) else sessions)
finally:
    stop_events.set()
    server.terminate()
    try: server.wait(timeout=10)
    except Exception: server.kill()
    fakes.terminate()
    result["event_types"] = sorted({e.get("type", "?") for e in events})
    json.dump(result, open(f"{RUN}/result.json", "w"), indent=1)
    print("done ->", RUN)
