"""Scenario tests for m31ssh. Driven by test_m31ssh.sh; see its header.

Synchronisation is always on observable output (a pty read that matches a
pattern, a process exit, a socket accept) with a generous timeout that only
bounds a failure -- never a sleep standing in for a condition.
"""
import errno, fcntl, hashlib, os, pty, re, select, shutil, signal, socket
import struct, subprocess, sys, tempfile, termios, threading, time

BIN = sys.argv[1]
SSHD = sys.argv[2]
W = tempfile.mkdtemp(prefix="m31ssh-test-")
HOME = os.path.join(W, "home")
os.makedirs(os.path.join(HOME, ".ssh"), mode=0o700)
KH = os.path.join(HOME, ".ssh", "known_hosts")
USER = subprocess.run(["id", "-un"], capture_output=True, text=True).stdout.strip()

procs = []
passed = failed = 0


def ok(name, cond, detail=""):
    global passed, failed
    if cond:
        passed += 1
        print("ok   " + name, flush=True)
    else:
        failed += 1
        print("FAIL " + name, flush=True)
        if detail:
            print("     " + str(detail)[:600].replace("\n", "\n     "), flush=True)


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def keygen(path, comment="t"):
    subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", comment, "-f", path], check=True)
    os.chmod(path, 0o600)


def fingerprint(pubpath):
    out = subprocess.run(["ssh-keygen", "-lf", pubpath], capture_output=True, text=True).stdout
    return out.split()[1]


# --- fixtures -----------------------------------------------------------------
HOSTKEY = os.path.join(W, "host_key")
USERKEY = os.path.join(HOME, ".ssh", "id_ed25519")
OTHERKEY = os.path.join(W, "other_key")
keygen(HOSTKEY, "host")
keygen(USERKEY, "user")
keygen(OTHERKEY, "other")
shutil.copy(USERKEY + ".pub", os.path.join(W, "authorized_keys"))
PORT = free_port()
with open(os.path.join(W, "sshd_config"), "w") as f:
    f.write(f"""Port {PORT}
ListenAddress 127.0.0.1
HostKey {HOSTKEY}
PidFile {W}/sshd.pid
AuthorizedKeysFile {W}/authorized_keys
StrictModes no
UsePAM no
KexAlgorithms curve25519-sha256
HostKeyAlgorithms ssh-ed25519
Ciphers chacha20-poly1305@openssh.com
PubkeyAuthentication yes
PasswordAuthentication no
KbdInteractiveAuthentication no
PrintMotd no
PrintLastLog no
""")
sshd = subprocess.Popen([SSHD, "-f", os.path.join(W, "sshd_config"), "-D", "-e"],
                        stderr=subprocess.PIPE, stdout=subprocess.DEVNULL)
procs.append(sshd)
listening = threading.Event()


def drain_sshd():
    for line in sshd.stderr:
        if b"listening on" in line:
            listening.set()


threading.Thread(target=drain_sshd, daemon=True).start()
if not listening.wait(20):
    print("FAIL sshd did not start")
    sys.exit(1)

ENV = {"HOME": HOME, "PATH": os.environ.get("PATH", "/usr/bin:/bin"), "USER": USER,
       "LOGNAME": USER, "TERM": "xterm"}
KH_OPT = ["-o", "UserKnownHostsFile=" + KH]


def trust_host():
    with open(KH, "w") as f:
        f.write("[127.0.0.1]:%d %s" % (PORT, " ".join(open(HOSTKEY + ".pub").read().split()[:2])) + "\n")


def run(args, input=None, env=None, timeout=60, binary=False):
    e = dict(ENV)
    if env:
        e.update(env)
    p = subprocess.run([BIN] + args, input=input, capture_output=True, env=e, timeout=timeout)
    if binary:
        return p
    return p.returncode, p.stdout.decode(errors="replace"), p.stderr.decode(errors="replace")


def base(*args):
    """Options (with their values) up to the first plain word, then the host,
    then the rest as the remote command."""
    args = list(args)
    i = 0
    while i < len(args) and args[i].startswith("-"):
        i += 2 if args[i] in ("-o", "-i", "-p", "-l", "-F", "-e", "-J", "-L", "-R") else 1
    return (["-p", str(PORT), "-o", "UserKnownHostsFile=" + KH] + args[:i]
            + ["127.0.0.1"] + args[i:])


class Pty:
    """m31ssh running on a pty, with expect/send."""

    def __init__(self, args, env=None, rows=24, cols=80):
        e = dict(ENV)
        if env:
            e.update(env)
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

        def child():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        self.proc = subprocess.Popen([BIN] + args, stdin=slave, stdout=slave, stderr=slave,
                                     env=e, preexec_fn=child, close_fds=True)
        os.close(slave)
        procs.append(self.proc)
        self.buf = b""
        self.total = b""

    def expect(self, pattern, timeout=30):
        rx = re.compile(pattern.encode() if isinstance(pattern, str) else pattern)
        end = time.monotonic() + timeout
        while True:
            m = rx.search(self.buf)
            if m:
                out = self.buf[:m.end()]
                self.buf = self.buf[m.end():]
                return out.decode(errors="replace")
            left = end - time.monotonic()
            if left <= 0:
                raise TimeoutError("expect %r; have %r" % (pattern, self.buf[-300:]))
            r, _, _ = select.select([self.master], [], [], left)
            if not r:
                continue
            try:
                d = os.read(self.master, 65536)
            except OSError:
                d = b""
            if not d:
                raise EOFError("eof waiting for %r; have %r" % (pattern, self.buf[-300:]))
            self.buf += d
            self.total += d

    def send(self, data):
        os.write(self.master, data if isinstance(data, bytes) else data.encode())

    def resize(self, rows, cols):
        fcntl.ioctl(self.master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        try:
            os.killpg(self.proc.pid, signal.SIGWINCH)
        except OSError:
            pass

    def drain(self, timeout=10):
        """Read until the child's side closes; returns all output."""
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            r, _, _ = select.select([self.master], [], [], 0.2)
            if r:
                try:
                    d = os.read(self.master, 65536)
                except OSError:
                    break
                if not d:
                    break
                self.buf += d
                self.total += d
            elif self.proc.poll() is not None:
                break
        return self.buf.decode(errors="replace")

    def finish(self, timeout=15):
        try:
            code = self.proc.wait(timeout)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            code = self.proc.wait()
            code = "hang"
        self.drain(1)
        return code

    def echo_on(self):
        a = termios.tcgetattr(self.master)
        return bool(a[3] & termios.ECHO) and bool(a[3] & termios.ICANON)

    def close(self):
        try:
            os.close(self.master)
        except OSError:
            pass


def shell_ready(p):
    """Gets a predictable POSIX shell with a unique marker."""
    p.send("exec sh\n")
    p.send("PS1=; echo READY$((6*7))\n")
    p.expect(r"READY42")


def scenario(name):
    def deco(fn):
        try:
            fn()
        except Exception as e:  # a harness failure is a test failure
            ok(name + " (exception)", False, repr(e))
        return fn
    return deco


# --- non-interactive ------------------------------------------------------------
trust_host()
code, out, err = run(base("echo", "hello", "world"))
ok("exec: output and exit 0", code == 0 and out == "hello world\n", (code, out, err))
code, out, err = run(base("sh", "-c", "'echo out; echo err >&2; exit 7'"))
ok("exec: remote exit status 7 propagated", code == 7, (code, out, err))
ok("exec: stdout and stderr stay separate", out == "out\n" and "err" in err and "out" not in err, (out, err))

n = 6 * 1000 * 1000
data = os.urandom(n)
p = run(base("cat"), input=data, binary=True, timeout=120)
ok("large data both ways: 6 MB through `cat` is intact",
   p.returncode == 0 and hashlib.sha256(p.stdout).digest() == hashlib.sha256(data).digest(), (p.returncode, len(p.stdout)))
code, out, err = run(base("seq", "1", "1500000"), timeout=120)
want = "".join("%d\n" % i for i in range(1, 1500001))
ok("large output: 1.5M lines (about 10 MB) from the server", code == 0 and out == want, (code, len(out)))
p = run(base("wc", "-c"), input=b"x" * 3000000, binary=True, timeout=120)
ok("large input: 3 MB to `wc -c`", p.stdout.decode().strip() == "3000000", p.stdout)
code, out, err = run(base("cat"), input=b"piped\n")
ok("stdin EOF reaches the remote command", code == 0 and out == "piped\n", (code, out, err))
code, out, err = run(base("-n", "cat"))
ok("-n: remote reads EOF at once", code == 0 and out == "", (code, out, err))
code, out, err = run(base("tty"))
ok("no pty without -t when stdin is not a terminal", "not a tty" in out, (code, out))
code, out, err = run(base("-tt", "tty"))
ok("-tt forces a pty", code == 0 and "/dev/" in out and "not a tty" not in out, (code, out, err))
code, out, err = run(["--help"])
ok("--help says UNAUDITED", code == 0 and "UNAUDITED" in out, out)
code, out, err = run(["-V"])
ok("-V prints version and UNAUDITED", code == 0 and "UNAUDITED" in out and "m31ssh" in out, out)
code, out, err = run(["-p", "99999", "h"])
ok("bad port is a one-line error with exit 255", code == 255 and len(err.strip().splitlines()) <= 2, (code, err))

# --- interactive over a pty ---------------------------------------------------------
@scenario("interactive")
def _():
    p = Pty(base(), rows=24, cols=80)
    shell_ready(p)
    p.send("stty size\n")
    ok("pty: initial size 24x80 reaches the server", "24 80" in p.expect(r"24 80"))
    p.resize(41, 117)
    seen = False
    for _ in range(100):
        p.send("stty size\n")
        try:
            p.expect(r"41 117", timeout=2)
            seen = True
            break
        except TimeoutError:
            continue
    ok("pty: resize propagates (stty size 41 117)", seen)
    p.send("echo TERM=$TERM\n")
    ok("pty: TERM is passed", "TERM=xterm" in p.expect(r"TERM=xterm"))
    p.send("exit 3\n")
    code = p.finish()
    ok("pty: exit status 3 is the client's exit status", code == 3, code)
    ok("pty: terminal restored (echo and canonical on)", p.echo_on())
    p.close()

@scenario("ctrl-d")
def _():
    p = Pty(base())
    shell_ready(p)
    p.send(b"\x04")
    code = p.finish()
    ok("Ctrl-D ends the remote shell and the client exits 0", code == 0, code)
    ok("Ctrl-D: terminal restored", p.echo_on())
    p.close()

@scenario("tilde-dot")
def _():
    p = Pty(base())
    shell_ready(p)
    p.send("\r")
    p.send("~.")
    code = p.finish()
    ok("~. at line start disconnects with 255", code == 255, code)
    ok("~. prints a closed notice", "closed" in p.total.decode(errors="replace"), p.total[-200:])
    ok("~. restores the terminal", p.echo_on())
    p.close()

@scenario("tilde-not-at-line-start")
def _():
    p = Pty(base())
    shell_ready(p)
    p.send("echo a~.b\n")
    p.expect(r"a~\.b\r?\n")
    ok("~. in the middle of a line is ordinary text", p.proc.poll() is None)
    p.send(b"\x04")
    ok("...and the session still ends cleanly", p.finish() == 0)
    p.close()

@scenario("big-output-pty")
def _():
    p = Pty(base())
    shell_ready(p)
    p.send("seq 1 200000; echo DONE$((1+1))\n")
    out = p.expect(r"DONE2", timeout=60)
    out = re.sub(r"\x1b\[[0-9;?]*[a-zA-Z]", "", out)
    nums = [l for l in out.replace("\r", "").split("\n") if l.isdigit()]
    ok("pty: 200000 lines of output arrive in order", nums[:3] == ["1", "2", "3"] and nums[-1] == "200000" and len(nums) == 200000, len(nums))
    p.send("exit\n")
    p.finish()
    p.close()

@scenario("ctrl-c-passthrough")
def _():
    p = Pty(base())
    shell_ready(p)
    p.send("sleep 300\n")
    p.send(b"\x03")
    p.expect(r"\^C")
    p.send("echo AFTER$((2+3))\n")
    p.expect(r"AFTER5")
    ok("Ctrl-C reaches the remote command, not the client", p.proc.poll() is None)
    p.send("exit\n")
    p.finish()
    p.close()

# --- host keys -------------------------------------------------------------------------
fp = fingerprint(HOSTKEY + ".pub")
pre = open(KH).read()
bad_kh = os.path.join(W, "bad_known_hosts")
other_pub = open(OTHERKEY + ".pub").read().split()
with open(bad_kh, "w") as f:
    f.write("[127.0.0.1]:%d %s %s\n" % (PORT, other_pub[0], other_pub[1]))
before = open(bad_kh, "rb").read()
code, out, err = run(["-p", str(PORT), "-o", "UserKnownHostsFile=" + bad_kh, "127.0.0.1", "true"])
ok("changed host key: hard refusal, 255", code == 255, (code, err))
ok("changed host key: names the offered fingerprint", fp in err and "CHANGED" in err, err)
ok("changed host key: known_hosts never rewritten", open(bad_kh, "rb").read() == before)
ok("changed host key: no command ran", out == "")

new_kh = os.path.join(W, "new_known_hosts")
code, out, err = run(["-p", str(PORT), "-o", "UserKnownHostsFile=" + new_kh, "127.0.0.1", "true"])
ok("unknown host without a tty is refused, 255", code == 255 and not os.path.exists(new_kh), (code, err))
code, out, err = run(["-p", str(PORT), "-o", "UserKnownHostsFile=" + new_kh, "-o", "StrictHostKeyChecking=yes", "127.0.0.1", "true"])
ok("StrictHostKeyChecking=yes refuses unknown", code == 255 and not os.path.exists(new_kh), (code, err))
code, out, err = run(["-p", str(PORT), "-o", "UserKnownHostsFile=" + new_kh, "-o", "StrictHostKeyChecking=accept-new", "127.0.0.1", "echo", "in"])
ok("accept-new trusts on first use and records it", code == 0 and out == "in\n" and os.path.exists(new_kh), (code, err))
ok("recorded line has the right key", open(new_kh).read().split()[2] == open(HOSTKEY + ".pub").read().split()[1])
code, out, err = run(["-p", str(PORT), "-o", "UserKnownHostsFile=" + new_kh, "-o", "StrictHostKeyChecking=yes", "127.0.0.1", "echo", "again"])
ok("a recorded host then passes strict checking", code == 0 and out == "again\n", (code, err))

@scenario("tofu-accept")
def _():
    kh = os.path.join(W, "tofu_accept")
    p = Pty(["-p", str(PORT), "-o", "UserKnownHostsFile=" + kh, "127.0.0.1"])
    t = p.expect(r"\(yes/no\)\? ")
    ok("prompt shows the fingerprint", fp in t, t)
    p.send("yes\n")
    shell_ready(p)
    p.send("exit\n")
    ok("accepted: session works and exits 0", p.finish() == 0)
    ok("accepted: key was recorded", os.path.exists(kh) and fp.split(":")[0] == "SHA256")
    p.close()

@scenario("tofu-reject")
def _():
    kh = os.path.join(W, "tofu_reject")
    p = Pty(["-p", str(PORT), "-o", "UserKnownHostsFile=" + kh, "127.0.0.1"])
    p.expect(r"\(yes/no\)\? ")
    p.send("no\n")
    code = p.finish()
    ok("rejected: exit 255 and nothing recorded", code == 255 and not os.path.exists(kh), code)
    ok("rejected: terminal state is sane", p.echo_on())
    p.close()

# --- config files, keys, agent ---------------------------------------------------------------
cfg = os.path.join(HOME, ".ssh", "config")
os.unlink(USERKEY)  # from here on: no default key; only -i, config, or the agent
shutil.copy(USERKEY + ".pub", USERKEY + ".bak.pub")
real_key = os.path.join(W, "real_key")
shutil.copy(HOSTKEY, W + "/dummy")  # placeholder so the next line is clear
# the authorized user key, kept outside ~/.ssh
real = os.path.join(W, "authorized_user_key")
subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", real], check=True)
shutil.copy(real + ".pub", os.path.join(W, "authorized_keys"))
os.chmod(real, 0o600)

code, out, err = run(base("-i", real, "echo", "viaI"))
ok("-i identity authenticates", code == 0 and out == "viaI\n", (code, err))
code, out, err = run(base("-i", OTHERKEY, "echo", "x"))
ok("wrong key: Permission denied, 255", code == 255 and "Permission denied" in err, (code, err))
code, out, err = run(base("echo", "x"))
ok("no identity at all: clear error, 255", code == 255 and err.strip() != "", (code, err))
loose = os.path.join(W, "loose_key")
shutil.copy(real, loose)
os.chmod(loose, 0o644)
code, out, err = run(base("-i", loose, "echo", "x"))
ok("a group/world-readable key file is refused, not used", code == 255 and "accessible by others" in err, (code, err))

with open(cfg, "w") as f:
    f.write(f"""Host box
  HostName 127.0.0.1
  Port {PORT}
  User {USER}
  IdentityFile {real}
  UserKnownHostsFile {KH}
  StrictHostKeyChecking yes
Host *
  User nobody-matches
""")
code, out, err = run(["box", "echo", "viaconfig"])
ok("~/.ssh/config: Host/HostName/Port/User/IdentityFile/UserKnownHostsFile", code == 0 and out == "viaconfig\n", (code, err))
code, out, err = run(["-l", USER, "-o", "Port=%d" % PORT, "-o", "HostName=127.0.0.1", "-o", "IdentityFile=" + real, "-o", "UserKnownHostsFile=" + KH, "box", "echo", "cli"])
ok("command-line -o and -l take precedence over the file", code == 0 and out == "cli\n", (code, err))
code, out, err = run(["-F", "/nonexistent/config", "box", "true"])
ok("missing -F file is an error", code == 255, (code, err))

# agent
agent = subprocess.Popen(["ssh-agent", "-D", "-a", os.path.join(W, "agent.sock")],
                         stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
procs.append(agent)
agent.stdout.readline()
AENV = {"SSH_AUTH_SOCK": os.path.join(W, "agent.sock")}
os.unlink(cfg)
code, out, err = run(base("echo", "x"), env=AENV)
ok("agent with no keys: denied, no crash", code == 255, (code, err))
subprocess.run(["ssh-add", "-q", real], env={**os.environ, **AENV}, check=True)
code, out, err = run(base("echo", "signed-by-agent"), env=AENV)
ok("ssh-agent signs (no key file anywhere)", code == 0 and out == "signed-by-agent\n", (code, err))
code, out, err = run(base("-o", "IdentitiesOnly=yes", "echo", "x"), env=AENV)
ok("IdentitiesOnly=yes ignores unrelated agent keys", code == 255, (code, err))
code, out, err = run(base("-o", "IdentitiesOnly=yes", "-i", real, "echo", "both"), env=AENV)
ok("IdentitiesOnly with the matching file works", code == 0 and out == "both\n", (code, err))
code, out, err = run(base("echo", "x"), env={"SSH_AUTH_SOCK": os.path.join(W, "no-such.sock")})
ok("dead SSH_AUTH_SOCK: reported, not fatal, 255 (no other key)", code == 255, (code, err))

# --- keepalive and timeouts -----------------------------------------------------------------------
@scenario("keepalive")
def _():
    t0 = time.monotonic()
    p = subprocess.Popen([BIN] + base("-i", real, "-o", "ServerAliveInterval=1", "-o", "ServerAliveCountMax=2", "sleep", "60"),
                         env=ENV, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    procs.append(p)
    # Wait until the command is running, then freeze every process below
    # the listening sshd (the whole per-connection tree): the server stops
    # answering without closing the connection.
    def tree(pid):
        out = subprocess.run(["pgrep", "-P", str(pid)], capture_output=True, text=True).stdout.split()
        found = []
        for c in out:
            found.append(int(c))
            found += tree(int(c))
        return found

    kids = []
    end = time.monotonic() + 20
    while time.monotonic() < end:
        kids = tree(sshd.pid)
        running = False
        for k in kids:
            cmd = subprocess.run(["ps", "-o", "command=", "-p", str(k)], capture_output=True, text=True).stdout
            if cmd.strip().endswith("sleep 61"):
                running = True
        if running:
            break
        select.select([], [], [], 0.05)
    for k in kids:
        os.kill(k, signal.SIGSTOP)
    try:
        _, err = p.communicate(timeout=30)
        code = p.returncode
    except subprocess.TimeoutExpired:
        p.kill()
        code, err = "hang", b""
    for k in reversed(kids):
        try:
            os.kill(k, signal.SIGKILL)
        except OSError:
            pass
    ok("ServerAliveInterval/CountMax: silent server -> 255 'not responding'",
       code == 255 and b"not responding" in err, (code, err))
    ok("keepalive gave up in bounded time", time.monotonic() - t0 < 25)

@scenario("silent-peer")
def _():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    s.listen(4)
    port = s.getsockname()[1]
    t0 = time.monotonic()
    p = subprocess.run([BIN, "-p", str(port), "-o", "ConnectTimeout=1", "-o", "UserKnownHostsFile=" + KH,
                        "-i", real, "127.0.0.1", "true"], env=ENV, capture_output=True, timeout=30)
    ok("silent server: read timeout, one-line error, 255",
       p.returncode == 255 and time.monotonic() - t0 < 20 and len(p.stderr.decode().strip().splitlines()) == 1,
       (p.returncode, p.stderr))
    s.close()

# --- malicious servers: no crash, no hang -----------------------------------------------------------
def hostile(name, payload_fn):
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", 0))
    s.listen(1)
    port = s.getsockname()[1]

    def serve():
        try:
            c, _ = s.accept()
            payload_fn(c)
            c.close()
        except Exception:
            pass

    t = threading.Thread(target=serve, daemon=True)
    t.start()
    try:
        p = subprocess.run([BIN, "-p", str(port), "-o", "ConnectTimeout=3", "-o", "UserKnownHostsFile=" + KH,
                            "-i", real, "127.0.0.1", "true"], env=ENV, capture_output=True, timeout=30)
        code, err = p.returncode, p.stderr
    except subprocess.TimeoutExpired:
        code, err = "hang", b""
    s.close()
    ok("hostile server (%s): exit 255, no crash or hang" % name, code == 255 and err.strip() != b"", (code, err))


def u32(n):
    return struct.pack(">I", n)


hostile("garbage instead of a version", lambda c: c.sendall(os.urandom(5000)))
hostile("not SSH", lambda c: c.sendall(b"HTTP/1.1 200 OK\r\n\r\n"))
hostile("SSH-1.5", lambda c: c.sendall(b"SSH-1.5-old\r\n"))
hostile("endless version line", lambda c: c.sendall(b"SSH-2.0-" + b"A" * 70000))
hostile("version then random bytes", lambda c: (c.sendall(b"SSH-2.0-evil\r\n"), c.sendall(os.urandom(4000))))
hostile("version then huge packet length", lambda c: (c.sendall(b"SSH-2.0-evil\r\n"), c.sendall(u32(0x7fffffff) + b"\x00" * 64)))
hostile("version then truncated KEXINIT", lambda c: (c.sendall(b"SSH-2.0-evil\r\n"), c.sendall(u32(20) + b"\x08\x14" + b"\x00" * 18)))
hostile("KEXINIT with absurd name-list length",
        lambda c: (c.sendall(b"SSH-2.0-evil\r\n"), c.sendall(u32(60) + b"\x08\x14" + b"\x00" * 16 + u32(0xfffffff0) + b"\x00" * 36)))
hostile("immediate close after version", lambda c: c.sendall(b"SSH-2.0-evil\r\n"))
hostile("immediate close", lambda c: None)

# --- algorithms: every cipher, kex and host-key type against a restricted sshd ------------------------
def start_server(extra, hostkeys):
    """A second disposable sshd restricted by `extra` config lines."""
    port = free_port()
    cfgpath = os.path.join(W, "sshd_config_%d" % port)
    with open(cfgpath, "w") as f:
        f.write("Port %d\nListenAddress 127.0.0.1\nPidFile %s/sshd_%d.pid\nAuthorizedKeysFile %s/authorized_keys\n"
                "StrictModes no\nUsePAM no\nPubkeyAuthentication yes\nPasswordAuthentication no\n"
                "KbdInteractiveAuthentication no\n" % (port, W, port, W))
        for hk in hostkeys:
            f.write("HostKey %s\n" % hk)
        f.write(extra + "\n")
    pr = subprocess.Popen([SSHD, "-f", cfgpath, "-D", "-e"], stderr=subprocess.PIPE, stdout=subprocess.DEVNULL)
    procs.append(pr)
    up = threading.Event()

    def watch():
        for line in pr.stderr:
            if b"listening on" in line:
                up.set()

    threading.Thread(target=watch, daemon=True).start()
    if not up.wait(20):
        raise RuntimeError("sshd did not start: " + extra)
    return pr, port


ECKEY = os.path.join(W, "host_ecdsa")
RSAKEY = os.path.join(W, "host_rsa")
subprocess.run(["ssh-keygen", "-q", "-t", "ecdsa", "-b", "256", "-N", "", "-f", ECKEY], check=True)
subprocess.run(["ssh-keygen", "-q", "-t", "rsa", "-b", "2048", "-N", "", "-f", RSAKEY], check=True)
for k in (ECKEY, RSAKEY):
    os.chmod(k, 0o600)
payload = os.urandom(300000)


def algo_case(label, extra, hostkeys, expect_ok=True, expect_err=None):
    try:
        pr, port = start_server(extra, hostkeys)
    except Exception as e:
        ok(label + " (server)", False, repr(e))
        return
    kh = os.path.join(W, "kh_%d" % port)
    args = ["-p", str(port), "-o", "UserKnownHostsFile=" + kh, "-o", "StrictHostKeyChecking=accept-new", "-i", real, "127.0.0.1"]
    p = run(args + ["cat"], input=payload, binary=True, timeout=180)
    if expect_ok:
        ok(label + ": connects and moves 300 kB both ways intact",
           p.returncode == 0 and p.stdout == payload, (p.returncode, p.stderr[-300:]))
        # and again, strictly, now that the key is recorded
        c2, o2, e2 = run(["-p", str(port), "-o", "UserKnownHostsFile=" + kh, "-o", "StrictHostKeyChecking=yes",
                          "-i", real, "127.0.0.1", "echo", "again"])
        ok(label + ": recorded key passes strict checking", c2 == 0 and o2 == "again\n", (c2, e2))
    else:
        ok(label + ": refused with a clear one-line error",
           p.returncode == 255 and expect_err in p.stderr.decode(errors="replace")
           and len(p.stderr.decode().strip().splitlines()) == 1, (p.returncode, p.stderr))
    pr.kill()
    pr.wait()


HK_ED = [HOSTKEY]
algo_case("chacha20-poly1305", "Ciphers chacha20-poly1305@openssh.com", HK_ED)
algo_case("aes256-gcm", "Ciphers aes256-gcm@openssh.com", HK_ED)
algo_case("aes128-gcm", "Ciphers aes128-gcm@openssh.com", HK_ED)
algo_case("aes256-ctr + hmac-sha2-256-etm", "Ciphers aes256-ctr\nMACs hmac-sha2-256-etm@openssh.com", HK_ED)
algo_case("aes128-ctr + hmac-sha2-256 (MAC-then-encrypt)", "Ciphers aes128-ctr\nMACs hmac-sha2-256", HK_ED)
algo_case("kex ecdh-sha2-nistp256", "KexAlgorithms ecdh-sha2-nistp256", HK_ED)
algo_case("kex curve25519-sha256@libssh.org", "KexAlgorithms curve25519-sha256@libssh.org", HK_ED)
algo_case("host key ecdsa-sha2-nistp256", "HostKeyAlgorithms ecdsa-sha2-nistp256", [ECKEY])
algo_case("host key rsa-sha2-256", "HostKeyAlgorithms rsa-sha2-256", [RSAKEY])
algo_case("host key rsa-sha2-512", "HostKeyAlgorithms rsa-sha2-512", [RSAKEY])
algo_case("DOWNGRADE: only ssh-rsa (SHA-1) host key", "HostKeyAlgorithms ssh-rsa\nPubkeyAcceptedAlgorithms +ssh-rsa", [RSAKEY],
          expect_ok=False, expect_err="host key algorithms")
algo_case("DOWNGRADE: only aes128-cbc", "Ciphers aes128-cbc\nMACs hmac-sha2-256", HK_ED,
          expect_ok=False, expect_err="ciphers")
algo_case("DOWNGRADE: only diffie-hellman-group14-sha1", "KexAlgorithms diffie-hellman-group14-sha1", HK_ED,
          expect_ok=False, expect_err="key exchange")

@scenario("known-hosts-type-preference")
def _():
    pr, port = start_server("", [HOSTKEY, ECKEY, RSAKEY])
    # the file holds only the server's ECDSA key: the client must ask for it
    kh = os.path.join(W, "kh_pref")
    pub = open(ECKEY + ".pub").read().split()
    with open(kh, "w") as f:
        f.write("[127.0.0.1]:%d %s %s\n" % (port, pub[0], pub[1]))
    c, o, e = run(["-p", str(port), "-o", "UserKnownHostsFile=" + kh, "-o", "StrictHostKeyChecking=yes", "-i", real, "127.0.0.1", "echo", "ecdsa-pref"])
    ok("known_hosts holding only an ECDSA key: client asks for ECDSA and matches", c == 0 and o == "ecdsa-pref\n", (c, e))
    pub = open(RSAKEY + ".pub").read().split()
    with open(kh, "w") as f:
        f.write("[127.0.0.1]:%d %s %s\n" % (port, pub[0], pub[1]))
    c, o, e = run(["-p", str(port), "-o", "UserKnownHostsFile=" + kh, "-o", "StrictHostKeyChecking=yes", "-i", real, "127.0.0.1", "echo", "rsa-pref"])
    ok("known_hosts holding only an RSA key: client asks for RSA and matches", c == 0 and o == "rsa-pref\n", (c, e))
    # another server key type than the file's: refused as changed, never TOFU
    ed = open(HOSTKEY + ".pub").read().split()
    pr2, port2 = start_server("", [HOSTKEY])
    with open(kh, "w") as f:
        f.write("[127.0.0.1]:%d %s %s\n" % (port2, pub[0], pub[1]))
    c, o, e = run(["-p", str(port2), "-o", "UserKnownHostsFile=" + kh, "-o", "StrictHostKeyChecking=accept-new", "-i", real, "127.0.0.1", "echo", "x"])
    ok("file knows only an RSA key but the server presents ed25519: refused as changed, even with accept-new",
       c == 255 and "CHANGED" in e and open(kh).read().count("\n") == 1, (c, e))
    pr.kill()
    pr2.kill()

# --- tidy up ----------------------------------------------------------------------------------------
for pr in procs:
    try:
        if pr.poll() is None:
            pr.kill()
        pr.wait(5)
    except Exception:
        pass
shutil.rmtree(W, ignore_errors=True)
print("%d passed, %d failed" % (passed, failed))
sys.exit(1 if failed else 0)
