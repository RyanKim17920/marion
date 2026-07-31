#!/usr/bin/env python3
"""S2 pty host: spawn a TUI under a pty, log every raw output byte, drive a
scripted interaction (including a mid-session resize), then quit cleanly.

Log format: a simple length-prefixed record stream so we can reconstruct both
the raw byte stream and asciicast v3 timing.
  record := b'O' | b'I' | b'R'  + 8-byte double (monotonic ts) + 4-byte len + payload
"""
import fcntl, json, os, pty, select, signal, struct, sys, termios, time

def set_winsize(fd, rows, cols):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

def main():
    cfg = json.load(open(sys.argv[1]))
    out = open(cfg["log"], "wb")
    rows, cols = cfg["rows"], cfg["cols"]

    env = dict(os.environ)
    env.update({"TERM": cfg.get("term", "xterm-256color"), "LINES": str(rows),
                "COLUMNS": str(cols), "COLORTERM": "truecolor"})
    for k in ("CI", "NO_COLOR"):
        env.pop(k, None)
    for k in list(env):
        if any(k.startswith(p) for p in cfg.get("env_unset_prefix", [])):
            env.pop(k, None)
    env.update(cfg.get("env_set", {}))

    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(cfg["cwd"])
        set_winsize(0, rows, cols)
        os.execvpe(cfg["argv"][0], cfg["argv"], env)
        os._exit(127)
    set_winsize(fd, rows, cols)

    t0 = time.monotonic()
    def rec(tag, payload):
        out.write(tag + struct.pack("<dI", time.monotonic() - t0, len(payload)) + payload)
        out.flush()

    script = list(cfg["script"])  # [{at: sec, do: "send"|"resize"|"sigwinch"|"stop", ...}]
    deadline = cfg["duration"]

    while True:
        now = time.monotonic() - t0
        if now > deadline:
            break
        # fire due script steps
        while script and script[0]["at"] <= now:
            step = script.pop(0)
            act = step["do"]
            if act == "send":
                data = step["data"].encode().decode("unicode_escape").encode("utf-8")
                rec(b"I", data)
                os.write(fd, data)
            elif act == "resize":
                set_winsize(fd, step["rows"], step["cols"])
                rec(b"R", json.dumps([step["rows"], step["cols"]]).encode())
                os.kill(pid, signal.SIGWINCH)
            elif act == "stop":
                deadline = now
        timeout = 0.05
        try:
            r, _, _ = select.select([fd], [], [], timeout)
        except (OSError, ValueError):
            break
        if r:
            try:
                data = os.read(fd, 65536)
            except OSError:
                break
            if not data:
                break
            rec(b"O", data)
        if os.waitpid(pid, os.WNOHANG)[0] == pid:
            # drain
            try:
                while True:
                    r, _, _ = select.select([fd], [], [], 0.2)
                    if not r:
                        break
                    d = os.read(fd, 65536)
                    if not d:
                        break
                    rec(b"O", d)
            except OSError:
                pass
            break

    try:
        os.kill(pid, signal.SIGTERM)
        time.sleep(0.4)
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    try:
        os.waitpid(pid, 0)
    except ChildProcessError:
        pass
    out.close()
    print("captured", os.path.getsize(cfg["log"]), "bytes ->", cfg["log"])

main()
