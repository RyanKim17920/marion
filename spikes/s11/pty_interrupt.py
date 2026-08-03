#!/usr/bin/env python3
"""S11: replay S1's interrupt protocol over a **real pty** and record the framing.

S1 (`tests/fixtures/s1/`) proved Claude Code's `control_request`/`control_response`
interrupt protocol over **pipes**. Design §11 item 1 owes the pty re-confirmation: if the
CLI does isatty-conditional line buffering, the stream-json frames could arrive in
different chunks, or at different times, than the pipe capture shows.

This harness runs the *same* script over three transports and records, for each, every
`read()` chunk with a monotonic timestamp — so "same frames, same order, same boundaries"
is answerable from the recording rather than asserted.

  pipes    stdin/stdout/stderr are pipes                     (S1's transport; the control)
  pty-out  stdout is a pty; stdin and stderr are pipes        (isolates isatty(stdout))
  pty-in   stdin is a pty; stdout and stderr are pipes        (isolates isatty(stdin))
  pty-all  stdin+stdout+stderr are one pty, child owns it as its controlling terminal

The script is S1's, from `tests/fixtures/s1/stdin.jsonl`:
  initialize -> long turn -> interrupt mid-stream -> follow-up turn -> EOF.

Unlike `tests/fixtures/s2/ptyhost.py` (which drives a TUI, answers no terminal probes and
renders nothing) this host *watches* for DA1/XTVERSION/CPR probes on the output stream and
can answer them (`--answer-probes`), so "did anything need answering?" is measured rather
than assumed.

Usage:
    python3 pty_interrupt.py --transport pty-all --out DIR --base-url http://127.0.0.1:8111
"""
import argparse
import errno
import fcntl
import json
import os
import queue
import re
import select
import signal
import struct
import sys
import termios
import threading
import time

CLAUDE = os.environ.get("S11_CLAUDE", os.path.expanduser("~/.local/bin/claude"))
HARNESS_VERSION = "s11-pty_interrupt.py/1"

# S1's argv, verbatim from tests/fixtures/s1/summary.json.
S1_ARGV_TAIL = [
    "-p",
    "--output-format", "stream-json",
    "--input-format", "stream-json",
    "--include-partial-messages",
    "--verbose",
    "--model", "haiku",
    "--allowed-tools", "",
]

# S1's stdin script, verbatim from tests/fixtures/s1/stdin.jsonl (timings are event-driven
# here rather than wall-clock, which is the only deviation and is noted in the fixture).
INIT_FRAME = {"type": "control_request", "request_id": "req_1_init",
              "request": {"subtype": "initialize", "hooks": {}}}
LONG_PROMPT = ("Count slowly from 1 to 300, one number per line. "
               "Output nothing else. Do not stop early.")
INTERRUPT_FRAME = {"type": "control_request", "request_id": "req_2_interrupt",
                   "request": {"subtype": "interrupt"}}
FOLLOWUP_PROMPT = "Reply with exactly: OK-AFTER-INTERRUPT"


def user_frame(text):
    return {"type": "user", "session_id": "",
            "message": {"role": "user", "content": [{"type": "text", "text": text}]},
            "parent_tool_use_id": None}


# Terminal probes Claude Code is documented (§5.3) to emit.
PROBES = [
    ("DA1", re.compile(rb"\x1b\[c"), b"\x1b[?62;c"),
    ("DA2", re.compile(rb"\x1b\[>0?c"), b"\x1b[>0;10;1c"),
    ("XTVERSION", re.compile(rb"\x1b\[>0?q"), b"\x1bP>|s11 pty host\x1b\\"),
    ("CPR", re.compile(rb"\x1b\[6n"), b"\x1b[1;1R"),
    ("DECRQM-2026", re.compile(rb"\x1b\[\?2026\$p"), b"\x1b[?2026;2$y"),
]


def set_winsize(fd, rows, cols):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))


def disable_echo(fd, raw_output=False):
    """Echo off so our own stdin writes are not mirrored into the capture.

    `raw_output` additionally clears OPOST, which turns off ONLCR — the line
    discipline's LF -> CRLF translation. Leaving it on is the realistic default (that is
    what a terminal does); clearing it is how S11 attributes the `\\r` in the pty capture
    to the kernel rather than to the CLI.
    """
    a = termios.tcgetattr(fd)
    a[3] &= ~(termios.ECHO | termios.ECHOE | termios.ECHOK | termios.ECHONL)
    if raw_output:
        a[1] &= ~termios.OPOST
    termios.tcsetattr(fd, termios.TCSANOW, a)


class Recorder:
    """Length-prefixed raw record stream, same format as tests/fixtures/s2/ptyhost.py:
       tag(1) + monotonic double(8) + length u32(4) + payload."""

    def __init__(self, path, t0):
        self.fh = open(path, "wb")
        self.t0 = t0
        self.lock = threading.Lock()
        self.chunks = []          # (t, tag, nbytes)

    def rec(self, tag, payload):
        t = time.monotonic() - self.t0
        with self.lock:
            self.fh.write(tag + struct.pack("<dI", t, len(payload)) + payload)
            self.fh.flush()
            self.chunks.append((t, tag.decode(), len(payload)))
        return t

    def close(self):
        with self.lock:
            self.fh.close()


class Run:
    def __init__(self, args):
        self.args = args
        self.t0 = time.monotonic()
        self.out_dir = args.out
        os.makedirs(self.out_dir, exist_ok=True)
        self.recorder = Recorder(os.path.join(self.out_dir, "raw.bin"), self.t0)
        self.frames = queue.Queue()       # (t_arrival, chunk_index, obj, raw_line)
        self.frame_log = []
        self.probes_seen = []
        self.noise = []                   # non-JSON bytes seen on stdout
        self.buf = b""
        self.crlf_lines = 0
        self.lf_lines = 0
        self.chunk_index = 0
        self.stop = threading.Event()

    # -- spawn ------------------------------------------------------------
    def spawn(self):
        a = self.args
        argv = [CLAUDE] + S1_ARGV_TAIL

        env = dict(os.environ)
        env["TERM"] = "xterm-256color"
        env["COLUMNS"] = str(a.cols)
        env["LINES"] = str(a.rows)
        env["COLORTERM"] = "truecolor"
        for k in ("CI", "NO_COLOR", "FORCE_COLOR"):
            env.pop(k, None)
        env["ANTHROPIC_BASE_URL"] = a.base_url
        # §6.4: a non-empty API key silently wins over the auth token, so it is set empty.
        env["ANTHROPIC_AUTH_TOKEN"] = "s11-canned-not-a-credential"
        env["ANTHROPIC_API_KEY"] = ""
        self.env_note = sorted(["TERM", "COLUMNS", "LINES", "COLORTERM",
                                "ANTHROPIC_BASE_URL", "ANTHROPIC_AUTH_TOKEN",
                                "ANTHROPIC_API_KEY"])

        transport = a.transport
        pty_in = transport in ("pty-in", "pty-all")
        pty_out = transport in ("pty-out", "pty-all")

        master = slave = None
        if pty_in or pty_out:
            master, slave = os.openpty()
            set_winsize(slave, a.rows, a.cols)
            disable_echo(slave, raw_output=a.raw_output)

        in_r = in_w = out_r = out_w = err_r = err_w = None
        if not pty_in:
            in_r, in_w = os.pipe()
        if not pty_out:
            out_r, out_w = os.pipe()
        if transport != "pty-all":
            err_r, err_w = os.pipe()

        pid = os.fork()
        if pid == 0:
            try:
                os.setsid()
                if slave is not None:
                    fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
                os.dup2(slave if pty_in else in_r, 0)
                os.dup2(slave if pty_out else out_w, 1)
                os.dup2(slave if transport == "pty-all" else err_w, 2)
                for fd in (master, slave, in_r, in_w, out_r, out_w, err_r, err_w):
                    if fd is not None and fd > 2:
                        try:
                            os.close(fd)
                        except OSError:
                            pass
                os.chdir(self.args.cwd)
                os.execvpe(argv[0], argv, env)
            except Exception as e:  # pragma: no cover
                os.write(2, ("s11 exec failed: %r\n" % (e,)).encode())
            os._exit(127)

        self.pid = pid
        self.argv = argv
        for fd in (slave, in_r, out_w, err_w):
            if fd is not None:
                try:
                    os.close(fd)
                except OSError:
                    pass

        self.out_fd = master if pty_out else out_r
        self.in_fd = master if pty_in else in_w
        self.err_fd = None if transport == "pty-all" else err_r

        self.stderr_bytes = b""
        self.reader = threading.Thread(target=self._read_loop, daemon=True)
        self.reader.start()
        if self.err_fd is not None:
            self.err_thread = threading.Thread(target=self._err_loop, daemon=True)
            self.err_thread.start()

    # -- io ---------------------------------------------------------------
    def _read_loop(self):
        while not self.stop.is_set():
            try:
                r, _, _ = select.select([self.out_fd], [], [], 0.1)
            except (OSError, ValueError):
                break
            if not r:
                continue
            try:
                data = os.read(self.out_fd, 65536)
            except OSError as e:
                if e.errno in (errno.EIO,):   # pty closed by child exit
                    break
                break
            if not data:
                break
            t = self.rec(b"O", data)
            self.chunk_index += 1
            self._scan_probes(data, t)
            self._feed(data, t, self.chunk_index)

    def _err_loop(self):
        while not self.stop.is_set():
            try:
                r, _, _ = select.select([self.err_fd], [], [], 0.1)
            except (OSError, ValueError):
                break
            if not r:
                continue
            try:
                data = os.read(self.err_fd, 65536)
            except OSError:
                break
            if not data:
                break
            self.rec(b"E", data)
            self.stderr_bytes += data

    def _scan_probes(self, data, t):
        for name, rx, answer in PROBES:
            for m in rx.finditer(data):
                self.probes_seen.append({"t": t, "probe": name,
                                         "bytes": m.group(0).decode("latin1")})
                if self.args.answer_probes and self.args.transport != "pipes":
                    self.rec(b"I", answer)
                    os.write(self.in_fd, answer)

    def _feed(self, data, t, chunk_index):
        self.buf += data
        while b"\n" in self.buf:
            line, self.buf = self.buf.split(b"\n", 1)
            if line.endswith(b"\r"):
                self.crlf_lines += 1
            else:
                self.lf_lines += 1
            line = line.rstrip(b"\r")
            if not line.strip():
                continue
            try:
                obj = json.loads(line)
            except Exception:
                self.noise.append({"t": t, "bytes": line[:400].decode("latin1")})
                continue
            item = (t, chunk_index, obj, line)
            self.frame_log.append(item)
            self.frames.put(item)

    def rec(self, tag, payload):
        return self.recorder.rec(tag, payload)

    # -- script -----------------------------------------------------------
    def send(self, obj):
        raw = (json.dumps(obj) + "\n").encode()
        t = self.rec(b"I", raw)
        os.write(self.in_fd, raw)
        return t

    def wait_for(self, pred, timeout, what):
        end = time.monotonic() + timeout
        while True:
            remaining = end - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("timed out waiting for %s" % what)
            try:
                item = self.frames.get(timeout=min(remaining, 0.25))
            except queue.Empty:
                continue
            if pred(item[2]):
                return item


def is_control_response(rid):
    def f(o):
        return (o.get("type") == "control_response"
                and o.get("response", {}).get("request_id") == rid)
    return f


def is_first_delta(o):
    return (o.get("type") == "stream_event"
            and o.get("event", {}).get("type") == "content_block_delta")


def is_result(o):
    return o.get("type") == "result"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--transport", required=True,
                    choices=["pipes", "pty-out", "pty-in", "pty-all"])
    ap.add_argument("--out", required=True)
    ap.add_argument("--base-url", required=True)
    ap.add_argument("--cwd", default=os.getcwd())
    ap.add_argument("--rows", type=int, default=40)
    ap.add_argument("--cols", type=int, default=120)
    ap.add_argument("--answer-probes", action="store_true")
    ap.add_argument("--raw-output", action="store_true",
                    help="clear OPOST on the pty slave, disabling ONLCR (LF -> CRLF). "
                         "Attributes the \\r in a pty capture to the line discipline "
                         "rather than to the CLI.")
    ap.add_argument("--interrupt-after", type=float, default=3.0,
                    help="seconds after the first content delta")
    ap.add_argument("--timeout", type=float, default=90.0)
    args = ap.parse_args()

    run = Run(args)
    run.spawn()

    summary = {
        "harness_version": HARNESS_VERSION,
        "transport": args.transport,
        "argv": run.argv,
        "env_overrides": run.env_note,
        "answer_probes": bool(args.answer_probes),
        "raw_output_opost_cleared": bool(args.raw_output),
        "winsize": [args.rows, args.cols] if args.transport != "pipes" else None,
        "events": {},
        "errors": [],
    }

    try:
        t_init = run.send(INIT_FRAME)
        t, _, init_resp, _ = run.wait_for(is_control_response("req_1_init"),
                                          args.timeout, "initialize response")
        summary["events"]["initialize_latency_s"] = t - t_init
        summary["events"]["initialize_response_bytes"] = len(json.dumps(init_resp))

        t_prompt = run.send(user_frame(LONG_PROMPT))
        t_delta, _, first_delta, _ = run.wait_for(is_first_delta, args.timeout,
                                                  "first content delta")
        summary["events"]["first_delta_after_s"] = t_delta - t_prompt
        summary["events"]["first_delta"] = first_delta

        time.sleep(args.interrupt_after)
        t_int = run.send(INTERRUPT_FRAME)
        t_resp, _, int_resp, _ = run.wait_for(is_control_response("req_2_interrupt"),
                                              args.timeout, "interrupt response")
        summary["events"]["interrupt_response"] = int_resp
        summary["events"]["interrupt_response_latency_s"] = t_resp - t_int
        t_res, _, interrupted, _ = run.wait_for(is_result, args.timeout,
                                                "interrupted result")
        summary["events"]["interrupt_to_result_s"] = t_res - t_int
        summary["events"]["interrupted_result"] = {
            k: interrupted.get(k) for k in
            ("type", "subtype", "is_error", "terminal_reason", "num_turns",
             "duration_ms", "total_cost_usd", "errors")}

        t_follow = run.send(user_frame(FOLLOWUP_PROMPT))
        t_fres, _, followup, _ = run.wait_for(is_result, args.timeout, "follow-up result")
        summary["events"]["followup_latency_s"] = t_fres - t_follow
        summary["events"]["followup_result"] = {
            k: followup.get(k) for k in
            ("type", "subtype", "is_error", "terminal_reason", "result",
             "num_turns", "total_cost_usd")}
    except Exception as e:
        summary["errors"].append(repr(e))

    # EOF on stdin, then reap.
    try:
        if args.transport in ("pty-in", "pty-all"):
            os.write(run.in_fd, b"\x04")     # pty has no half-close; EOT ends the read
        else:
            os.close(run.in_fd)
    except OSError:
        pass

    exit_status = None
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        wpid, st = os.waitpid(run.pid, os.WNOHANG)
        if wpid == run.pid:
            exit_status = st
            break
        time.sleep(0.05)
    if exit_status is None:
        summary["errors"].append("child did not exit; SIGTERM sent")
        try:
            os.kill(run.pid, signal.SIGTERM)
            time.sleep(0.5)
            os.kill(run.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        try:
            _, exit_status = os.waitpid(run.pid, 0)
        except ChildProcessError:
            exit_status = -1
    time.sleep(0.4)          # let the reader drain
    run.stop.set()
    time.sleep(0.2)
    run.recorder.close()

    summary["exit_status"] = exit_status
    summary["exit_code"] = (os.waitstatus_to_exitcode(exit_status)
                            if isinstance(exit_status, int) and exit_status >= 0 else None)
    summary["stderr"] = run.stderr_bytes.decode("utf-8", "replace")[-4000:]
    summary["probes_seen"] = run.probes_seen
    summary["non_json_stdout"] = run.noise[:50]
    summary["non_json_stdout_count"] = len(run.noise)

    # Framing evidence.
    chunks = run.recorder.chunks
    out_chunks = [c for c in chunks if c[1] == "O"]
    summary["framing"] = {
        "stdout_read_chunks": len(out_chunks),
        "stdout_bytes": sum(c[2] for c in out_chunks),
        "json_frames": len(run.frame_log),
        "lines_ending_crlf": run.crlf_lines,
        "lines_ending_lf_only": run.lf_lines,
        # A frame that arrived split across >1 read() call has a chunk index greater than
        # the previous frame's by more than the number of frames completed in between; the
        # simplest honest measure is frames-per-chunk, recorded per frame in stdout.jsonl.
        "frames_sharing_a_chunk": len(run.frame_log) - len({c for _, c, _, _ in run.frame_log}),
    }

    with open(os.path.join(args.out, "chunks.jsonl"), "w") as fh:
        for t, tag, n in chunks:
            fh.write(json.dumps({"t": round(t, 6), "tag": tag, "bytes": n}) + "\n")
    with open(os.path.join(args.out, "stdout.jsonl"), "w") as fh:
        for t, ci, obj, line in run.frame_log:
            fh.write(json.dumps({"t_rel": round(t, 6), "chunk": ci, "msg": obj}) + "\n")
    with open(os.path.join(args.out, "summary.json"), "w") as fh:
        json.dump(summary, fh, indent=2, sort_keys=False)
    print(json.dumps({"transport": args.transport,
                      "frames": len(run.frame_log),
                      "chunks": len(out_chunks),
                      "probes": len(run.probes_seen),
                      "errors": summary["errors"]}))


if __name__ == "__main__":
    main()
