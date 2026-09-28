import json, subprocess, sys, os
# usage: acpclient.py <answer: allow|reject> <cwd> <agent cmd...>
answer, cwd, cmd = sys.argv[1], sys.argv[2], sys.argv[3:]
p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=open(os.getenv('ACP_STDERR','/dev/null'),'w'), cwd=cwd, text=True, bufsize=1)
nid = [0]
def send(o): p.stdin.write(json.dumps(o)+"\n"); p.stdin.flush()
def req(m, params):
    nid[0]+=1; send({"jsonrpc":"2.0","id":nid[0],"method":m,"params":params}); return nid[0]
def wait(i):
    for line in p.stdout:
        line=line.strip()
        if not line: continue
        print("<<", line[:1500], flush=True)
        o=json.loads(line)
        if o.get("method")=="session/request_permission":
            opts=o["params"]["options"]
            want = ("allow_once","allow_always") if answer=="allow" else ("reject_once","reject_always")
            opt=[x for x in opts if x["kind"] in want][0]
            tc=o["params"].get("toolCall",{})
            print("## permission kind=%s title=%s options=%s -> %s" % (tc.get("kind"), tc.get("title"), [x["kind"] for x in opts], opt["optionId"]), flush=True)
            send({"jsonrpc":"2.0","id":o["id"],"result":{"outcome":{"outcome":"selected","optionId":opt["optionId"]}}})
        elif "method" in o and "id" in o:
            send({"jsonrpc":"2.0","id":o["id"],"error":{"code":-32601,"message":"not supported"}})
        elif o.get("id")==i: return o
wait(req("initialize",{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":False,"writeTextFile":False},"terminal":False}}))
r=wait(req("session/new",{"cwd":cwd,"mcpServers":[]}))
sid=r["result"]["sessionId"]
if os.getenv("ACP_MODE"):
    print("## set mode:", json.dumps(wait(req("session/set_config_option",{"sessionId":sid,"configId":"mode","value":os.getenv("ACP_MODE")})))[:300], flush=True)
r=wait(req("session/prompt",{"sessionId":sid,"prompt":[{"type":"text","text":"S38RO write the file"}]}))
print("## prompt result:", json.dumps(r)[:300])
p.terminate()
