#!/usr/bin/env -S uv run --with pyte python3
"""Drive the real `gemini` TUI on a PTY under s30's environment, type `/mcp`, and record what the
operator would see. Raw bytes go to `tui-mcp.raw`, rendered screens (pyte, 117x43 like the
facade E2E's OPERATOR_SIZE) to `tui-mcp.screen.txt`. Every wait is bounded; the child is killed
and reaped at the end."""
import os, pty, select, signal, sys, time, fcntl, termios, struct

S30 = os.path.dirname(os.path.abspath(__file__))
COLS, ROWS = 117, 43
import pyte

class Screen(pyte.Screen):
    # pyte does not accept private-marked SGR (`CSI ? ... m`) which ink/gemini emits; tolerate it.
    def select_graphic_rendition(self, *attrs, private=False):
        if private:
            return
        super().select_graphic_rendition(*attrs)

def render(stream_bytes):
    screen = Screen(COLS, ROWS)
    stream = pyte.ByteStream(screen)
    stream.feed(stream_bytes)
    return "\n".join(line.rstrip() for line in screen.display).rstrip() + "\n"

def read_for(fd, seconds, sink):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        r, _, _ = select.select([fd], [], [], 0.2)
        if fd in r:
            try:
                data = os.read(fd, 65536)
            except OSError:
                return False
            if not data:
                return False
            sink.extend(data)
    return True

def wait_for(fd, sink, needle, seconds):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        if needle.encode() in bytes(sink):
            return True
        if not read_for(fd, 0.5, sink):
            return False
    return False

pid, fd = pty.fork()
if pid == 0:
    os.environ["TERM"] = "xterm-256color"
    os.execv(os.path.join(S30, "env.sh"), [os.path.join(S30, "env.sh"), "gemini"])

fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
raw = bytearray()
log = []

# First screen: whatever gemini paints first (it may be an auth or trust dialog).
read_for(fd, 8, raw)
log.append(("first screen", render(bytes(raw))))

# The composer echoes what is typed; `/mcp` opens the server list.
os.write(fd, b"/mcp")
read_for(fd, 2, raw)
os.write(fd, b"\r")
found = wait_for(fd, raw, "pencil", 20)
read_for(fd, 3, raw)
log.append(("after /mcp", render(bytes(raw))))

os.kill(pid, signal.SIGTERM)
deadline = time.monotonic() + 10
while time.monotonic() < deadline:
    read_for(fd, 0.2, raw)
    wpid, status = os.waitpid(pid, os.WNOHANG)
    if wpid == pid:
        break
else:
    os.kill(pid, signal.SIGKILL)
    _, status = os.waitpid(pid, 0)

with open(os.path.join(S30, "tui-mcp.raw"), "wb") as f:
    f.write(raw)
with open(os.path.join(S30, "tui-mcp.screen.txt"), "w") as f:
    for label, screen in log:
        f.write(f"===== {label} ({COLS}x{ROWS}) =====\n{screen}\n")
print("pencil on screen after /mcp:", found)
print("raw bytes:", len(raw), "child status:", status)
for label, screen in log:
    print(f"===== {label} =====")
    print(screen)
