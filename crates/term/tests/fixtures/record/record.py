"""Tiny PTY recorder for shepherd emulator fixtures.

Usage: record.py SCRIPT.json
SCRIPT: {"cmd": [...], "cwd": "...", "cols": 100, "rows": 30, "out": "prefix",
         "steps": [["wait", "regex", timeout], ["send", "text"], ["sleep", s],
                   ["resize", cols, rows], ["split"]]}
Writes <out>.<n>.bin per segment ("split" / "resize" start a new segment) and <out>.log with
the step timeline. Answers DA1 / DSR-CPR / kitty-keyboard / OSC 10/11 queries the way a
typical xterm-compatible terminal would, so the app does not stall waiting for replies.
"""
import fcntl, json, os, pty, re, select, signal, struct, sys, termios, time

spec = json.load(open(sys.argv[1]))
cols, rows = spec["cols"], spec["rows"]
env = dict(os.environ)
env.update({"TERM": "xterm-256color", "COLORTERM": "truecolor", "LANG": "en_US.UTF-8"})
for k in ["TMUX", "TERM_PROGRAM", "TERM_PROGRAM_VERSION", "ITERM_SESSION_ID", "KITTY_WINDOW_ID",
          "WEZTERM_PANE", "GHOSTTY_RESOURCES_DIR", "VSCODE_PID"]:
    env.pop(k, None)
for k in list(env):
    if k.startswith("CLAUDE") or k.startswith("CODEX") or k.startswith("SHEPHERD"):
        env.pop(k)
env.update(spec.get("env", {}))

pid, fd = pty.fork()
if pid == 0:
    os.chdir(spec["cwd"])
    os.execvpe(spec["cmd"][0], spec["cmd"], env)

def setsize(c, r):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", r, c, 0, 0))
setsize(cols, rows)
os.kill(pid, signal.SIGWINCH)

segments = [bytearray()]
screen_text = bytearray()  # everything, for wait-regex matching
log = open(spec["out"] + ".log", "w")
t0 = time.time()

def note(msg):
    log.write(f"{time.time()-t0:7.2f} seg{len(segments)-1} {msg}\n"); log.flush()

QUERIES = [
    (rb"\x1b\[c|\x1b\[0c", lambda m: b"\x1b[?62;22c"),
    (rb"\x1b\[6n", lambda m: b"\x1b[1;1R"),
    (rb"\x1b\[\?u", lambda m: b"\x1b[?0u"),
    (rb"\x1b\]10;\?(\x07|\x1b\\)", lambda m: b"\x1b]10;rgb:cccc/cccc/cccc\x1b\\"),
    (rb"\x1b\]11;\?(\x07|\x1b\\)", lambda m: b"\x1b]11;rgb:1e1e/1e1e/1e1e\x1b\\"),
]

def pump(timeout):
    end = time.time() + timeout
    while True:
        left = end - time.time()
        if left <= 0:
            return True
        r, _, _ = select.select([fd], [], [], left)
        if not r:
            return True
        try:
            data = os.read(fd, 65536)
        except OSError:
            return False
        if not data:
            return False
        segments[-1].extend(data)
        screen_text.extend(data)
        for pat, reply in QUERIES:
            for m in re.finditer(pat, data):
                os.write(fd, reply(m)); note(f"answered {m.group(0)!r}")
        return True

alive = True
mark = 0
for step in spec["steps"]:
    kind = step[0]
    note(f"step {step!r}")
    if kind == "wait":
        pat = re.compile(step[1].encode())
        deadline = time.time() + step[2]
        while time.time() < deadline and alive:
            plain = re.sub(rb"\s+|\x1b\[[0-9;?<>=:]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(\x07|\x1b\\)", b"", bytes(screen_text[mark:]))
            if pat.search(plain):
                break
            alive = pump(0.2)
        else:
            note(f"TIMEOUT waiting for {step[1]}")
    elif kind == "mark":
        mark = len(screen_text)
    elif kind == "send":
        if not alive:
            note("skipped send: child exited"); continue
        try:
            os.write(fd, step[1].encode())
        except OSError:
            alive = False
    elif kind == "sleep":
        end = time.time() + step[1]
        while time.time() < end and alive:
            alive = pump(end - time.time())
    elif kind == "resize":
        segments.append(bytearray())
        setsize(step[1], step[2]); os.kill(pid, signal.SIGWINCH)
    elif kind == "split":
        segments.append(bytearray())

# drain
while alive and pump(1.5):
    if time.time() - t0 > 600:
        break
    alive = pump(1.5)
    break
try:
    os.kill(pid, signal.SIGTERM)
except ProcessLookupError:
    pass
for i, seg in enumerate(segments):
    open(f"{spec['out']}.{i}.bin", "wb").write(seg)
    note(f"wrote segment {i}: {len(seg)} bytes")
