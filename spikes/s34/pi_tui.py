"""S34/pi: pi's interactive TUI on a pty — startup dialogs, bracketed paste, and idle output.

    python3 spikes/s34/pi_tui.py <out-dir>

The TUI runs on a canned PI_CODING_AGENT_DIR with marion's extension loaded, just as the native
lane would load it. It is driven on a 120x40 pty. The script records the DEC private modes pi
turns on, the bytes it writes while idle and while busy, and whether a bracketed paste followed
by CR reaches the provider as a turn.
"""
import json, os, pty, re, select, socket, subprocess, sys, tempfile, time, fcntl, struct, termios

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
TEMPLATE = os.path.join(REPO, "crates/marion-harness/src/pi_extension.js")
OUT = os.path.abspath(sys.argv[1])


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


d = tempfile.mkdtemp(prefix="s34-tui-")
agent = os.path.join(d, "agent"); work = os.path.join(d, "work")
os.makedirs(agent); os.makedirs(work)
port = free_port()
json.dump({"providers": {"marion": {"baseUrl": f"http://127.0.0.1:{port}/v1", "api": "openai-completions",
                                    "apiKey": "k", "models": [{"id": "canned-1"}]}}}, open(os.path.join(agent, "models.json"), "w"))
reqlog = os.path.join(d, "requests.jsonl")
prov = subprocess.Popen([sys.executable, os.path.join(HERE, "canned_provider.py"), str(port), reqlog],
                        env=dict(os.environ, TOOL="mcp__marion__report", ARGS=json.dumps({"narrative": "tui report"}), TEXT="TUI-DONE"),
                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
server = {"command": sys.executable, "args": [os.path.join(HERE, "mcp_report_server.py"), os.path.join(d, "mcp.jsonl"), "slow"], "env": {}}
ext = os.path.join(d, "marion-pi.js")
open(ext, "w").write(open(TEMPLATE).read().replace("__MARION_SERVER__", json.dumps(server)).replace("__MARION_PREFIX__", '"mcp__marion__"'))

pid, fd = pty.fork()
if pid == 0:
    os.chdir(work)
    os.environ.update(PI_CODING_AGENT_DIR=agent, PI_SKIP_VERSION_CHECK="1", TERM="xterm-256color")
    os.execvp("pi", ["pi", "--no-extensions", "--provider", "marion", "--model", "canned-1",
                     "--tools", "read,mcp__marion__report", "-e", ext])
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
log = []  # (t, bytes)
t0 = time.time()


def pump(secs):
    end = time.time() + secs
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try:
                b = os.read(fd, 65536)
            except OSError:
                return
            log.append((time.time() - t0, b))


def since(t):
    return b"".join(b for (u, b) in log if u >= t)


def gaps(t_from, t_to):
    ts = [u for (u, _) in log if t_from <= u <= t_to]
    return [round(b - a, 3) for a, b in zip(ts, ts[1:])]


# Boot: until pi has drawn something and then been quiet for 2 s (bounded at 30 s).
end = time.time() + 30
while time.time() < end:
    pump(0.5)
    if log and time.time() - t0 - log[-1][0] > 2.0:
        break
boot = since(0)
t_boot_done = round(log[-1][0], 2) if log else None
t_first_byte = round(log[0][0], 2) if log else None
t_idle = time.time() - t0
pump(10)
idle = since(t_idle)
idle_writes = [(round(u - t_idle, 2), len(b)) for (u, b) in log if u >= t_idle]
t_paste = time.time() - t0
os.write(fd, b"\x1b[200~paste ZEBRA\nsecond line\x1b[201~")
time.sleep(0.05)
os.write(fd, b"\r")
pump(1.5)
t_busy_steer = time.time() - t0
os.write(fd, b"\x1b[200~steer while busy OKAPI\x1b[201~\r")
pump(10)
t_after = time.time() - t0
pump(4)
quiet_after = since(t_after)
os.write(fd, b"\x03")
pump(1)
os.write(fd, b"\x03")
pump(2)
try:
    os.kill(pid, 9)
except ProcessLookupError:
    pass
prov.kill()

modes = sorted(set(re.findall(rb"\x1b\[\?([0-9;]+[hl])", b"".join(b for _, b in log))))
osc = sorted(set(m[:24] for m in re.findall(rb"\x1b\](\d+;[^\x07\x1b]{0,40})", b"".join(b for _, b in log))))
reqs = [json.loads(l)["body"] for l in open(reqlog)] if os.path.exists(reqlog) else []
facts = {
    "decset_modes_seen_at_boot": [m.decode() for m in modes],
    "osc_sequences_seen": [o.decode(errors="replace") for o in osc],
    "first_byte_s": t_first_byte,
    "boot_settled_s": t_boot_done,
    "boot_bytes": len(boot),
    "idle_bytes_over_10s": len(idle),
    "idle_writes": idle_writes[:20],
    "bytes_in_4s_after_turn": len(quiet_after),
    "max_gap_while_busy_s": max(gaps(t_paste, t_after) or [0]),
    "requests": [[(m["role"], m["content"] if isinstance(m["content"], str) else
                   " ".join(c.get("text", "") for c in (m["content"] or []) if isinstance(c, dict)))
                  for m in r["messages"] if m["role"] != "system"] for r in reqs],
    "screen_text_mentions": {k: (k.encode() in b"".join(b for _, b in log)) for k in
                             ["TUI-DONE", "Trust", "trust", "changelog", "What's new", "login", "Login", "mcp__marion__report"]},
}
os.makedirs(OUT, exist_ok=True)
open(os.path.join(OUT, "pi-tui.facts.json"), "w").write(json.dumps(facts, indent=1) + "\n")
print(json.dumps(facts, indent=1))
