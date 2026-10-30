"""Smoke-drive the real shepherd binary in a pty with an isolated HOME."""
import os, pty, select, signal, subprocess, sys, tempfile, time, struct, fcntl, termios

BIN = sys.argv[1]
home = tempfile.mkdtemp(prefix="shsmoke")
os.chmod(home, 0o700)
env = {"HOME": home, "SHELL": "/bin/sh", "PATH": "/usr/bin:/bin", "TERM": "xterm-256color"}
CTRL_B = b"\x02"
log = []

def spawn():
    pid, fd = pty.fork()
    if pid == 0:
        os.execve(BIN, [BIN], env)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
    return pid, fd

def read_for(fd, secs):
    out = b""
    end = time.time() + secs
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                break
            if not chunk:
                break
            out += chunk
    return out

def step(name, ok, detail=""):
    log.append((name, ok, detail))
    print(("PASS " if ok else "FAIL ") + name + (f"  [{detail}]" if detail else ""), flush=True)

pid, fd = spawn()
out = read_for(fd, 3)
step("client starts and enters alternate screen", b"\x1b[?1049h" in out, f"{len(out)} bytes")
sock = os.path.join(home, ".shepherd", "host.sock")
step("host auto-launched (socket exists)", os.path.exists(sock))
mode = oct(os.stat(os.path.join(home, ".shepherd")).st_mode & 0o777) if os.path.exists(os.path.join(home, ".shepherd")) else "missing"
step("~/.shepherd is 0700", mode == "0o700", mode)

os.write(fd, CTRL_B + b"n"); out = read_for(fd, 2)
os.write(fd, b"echo smoke-one-$((40+2))\r"); out = read_for(fd, 2)
step("session 1 runs a shell and echoes", b"smoke-one-42" in out)

os.write(fd, CTRL_B + b"n"); read_for(fd, 2)
os.write(fd, b"echo smoke-two-$((50+5))\r"); out = read_for(fd, 2)
step("session 2 runs and echoes", b"smoke-two-55" in out)

os.write(fd, CTRL_B + b"j"); out = read_for(fd, 2)
step("switching sessions redraws session 1's output", b"one" in out and b"42" in out, "ratatui redraws only changed cells")

os.write(fd, CTRL_B + b"q"); out = read_for(fd, 2)
try:
    wpid, status = os.waitpid(pid, os.WNOHANG)
except ChildProcessError:
    wpid, status = pid, 0
step("q detaches (client exits)", wpid == pid, f"status={status}")
step("terminal restored on detach (left alternate screen)", b"\x1b[?1049l" in out)
step("host still running after detach", os.path.exists(sock))

pid2, fd2 = spawn()
out = read_for(fd2, 3)
step("reattach shows existing sessions' content", b"smoke-one-42" in out or b"smoke-two-55" in out)
os.write(fd2, CTRL_B + b"q"); read_for(fd2, 2)
try: os.waitpid(pid2, 0)
except ChildProcessError: pass

# Stop the host: find its pid via lsof on the socket (exact pid, not a pattern kill).
r = subprocess.run(["/usr/sbin/lsof", "-t", sock], capture_output=True, text=True)
pids = [int(p) for p in r.stdout.split()]
for p in pids:
    os.kill(p, signal.SIGTERM)
time.sleep(1.5)
step("host exits on SIGTERM and removes its socket", pids != [] and not os.path.exists(sock), f"pids={pids}")
hostlog = os.path.join(home, ".shepherd", "host.log")
step("host.log exists and is 0600", os.path.exists(hostlog) and (os.stat(hostlog).st_mode & 0o777) == 0o600)
import shutil; shutil.rmtree(home, ignore_errors=True)
print("RESULT", "ALL PASS" if all(ok for _, ok, _ in log) else "SOME FAILED")
