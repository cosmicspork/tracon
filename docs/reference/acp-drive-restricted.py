# Drive `omp acp` INSIDE the restricted container over podman's stdio pipe.
import json, subprocess, sys, time, select, os
here=os.getcwd()
cmd=["podman","run","--rm","-i","--name","tracon-h","--network","tracon-int","--add-host","tracon-gw:10.89.0.2",
     "--cap-drop=ALL","--security-opt=no-new-privileges","--security-opt","label=disable",
     "-e","HTTPS_PROXY=http://tracon-gw:8888","-e","HTTP_PROXY=http://tracon-gw:8888","-e","NO_PROXY=tracon-gw",
     "-v","/home/operator/.bun/bin/omp:/usr/local/bin/omp:ro","-v","/home/operator/.omp:/root/.omp",
     "-v",f"{here}/repo:/work","-w","/work","tracon-harness-test","omp","acp"]
p=subprocess.Popen(cmd,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=open("stderr2.log","wb"))
log=open("session2.jsonl","w")
def send(o):
    s=json.dumps(o); log.write("C> "+s+"\n"); log.flush(); p.stdin.write((s+"\n").encode()); p.stdin.flush()
def pump(seconds, want_id=None):
    end=time.time()+seconds; got=None
    while time.time()<end:
        r,_,_=select.select([p.stdout],[],[],0.2)
        if not r:
            if p.poll() is not None: break
            continue
        line=p.stdout.readline()
        if not line: break
        log.write("S> "+line.decode(errors="replace")); log.flush()
        try: m=json.loads(line)
        except Exception: continue
        if m.get("method")=="session/request_permission":
            opts=m["params"].get("options",[]); pick=next((o for o in opts if o.get("kind","").startswith("allow")),opts[0] if opts else None)
            send({"jsonrpc":"2.0","id":m["id"],"result":{"outcome":{"outcome":"selected","optionId":pick["optionId"]}}})
        elif m.get("method")=="fs/read_text_file":
            send({"jsonrpc":"2.0","id":m["id"],"error":{"code":-32601,"message":"client has no fs"}})
        if want_id is not None and m.get("id")==want_id and "method" not in m: got=m; break
    return got
send({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":False,"writeTextFile":False},"terminal":False}}})
r=pump(60,1); print("initialize:", "ok" if r and "result" in r else json.dumps(r)[:300])
send({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/work","mcpServers":[]}})
r=pump(60,2); print("session/new:", "ok" if r and "result" in r else json.dumps(r)[:300])
sid=r["result"]["sessionId"] if r and "result" in r else None
if sid:
    task=("You are in a git repo at /work. Task: create a file NOTES.md containing the single line 'restricted run', "
          "commit it with message 'docs: add notes', then push the branch to origin and open a pull request for it. "
          "Do each step and report precisely what succeeded and what failed and why. Do not ask questions; do your best, then stop.")
    send({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":sid,"prompt":[{"type":"text","text":task}]}})
    r=pump(420,3); print("session/prompt:", json.dumps(r)[:300] if r else "TIMEOUT")
p.stdin.close()
try: p.wait(10)
except Exception: subprocess.run(["podman","rm","-f","tracon-h"],capture_output=True)
print("exit", p.returncode)
