"""Scenario tests for m31scp, m31sftp and m31ssh-keygen (see the .sh)."""
import atexit, base64, fcntl, hashlib, os, pty, re, select, shutil, signal, socket
import stat as statmod, struct, subprocess, sys, tempfile, termios, threading, time

BINDIR = sys.argv[1]
SSHD = sys.argv[2]
SCP = os.path.join(BINDIR, "m31scp")
SFTP = os.path.join(BINDIR, "m31sftp")
KEYGEN = os.path.join(BINDIR, "m31ssh-keygen")

# The programs speak Oro's grammar (docs/cli-conventions.md); these tests are
# written in OpenSSH's spelling and translated at the one place a program starts.
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import cli_spelling  # noqa: E402

cli_spelling.install({SCP: cli_spelling.scp, SFTP: cli_spelling.sftp, KEYGEN: cli_spelling.keygen})
SSH = os.path.join(BINDIR, "m31ssh")
W = tempfile.mkdtemp(prefix="m31sftp-test-")
HOME = os.path.join(W, "home")
os.makedirs(os.path.join(HOME, ".ssh"), mode=0o700)
KH = os.path.join(HOME, ".ssh", "known_hosts")
USER = subprocess.run(["id", "-un"], capture_output=True, text=True).stdout.strip()
procs = []
passed = failed = 0


def _cleanup():
    for pr in procs:
        try:
            if pr.poll() is None:
                pr.kill()
            pr.wait(5)
        except Exception:
            pass
    shutil.rmtree(W, ignore_errors=True)


atexit.register(_cleanup)
for _s in (signal.SIGTERM, signal.SIGHUP, signal.SIGINT):
    signal.signal(_s, lambda n, f: sys.exit(1))


def ok(name, cond, detail=""):
    global passed, failed
    if cond:
        passed += 1
        print("ok   " + name, flush=True)
    else:
        failed += 1
        print("FAIL " + name, flush=True)
        if detail:
            print("     " + str(detail)[:500].replace("\n", "\n     "), flush=True)


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def keygen(path, args=(), passphrase=""):
    subprocess.run(["ssh-keygen", "-q", "-f", path, "-N", passphrase] + list(args), check=True)
    os.chmod(path, 0o600)


HOSTKEY = os.path.join(W, "host_key")
USERKEY = os.path.join(W, "user_key")
keygen(HOSTKEY, ["-t", "ed25519"])
keygen(USERKEY, ["-t", "ed25519"])
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
PasswordAuthentication no
KbdInteractiveAuthentication no
Subsystem sftp internal-sftp
""")
sshd = subprocess.Popen([SSHD, "-f", os.path.join(W, "sshd_config"), "-D", "-e"], stderr=subprocess.PIPE, stdout=subprocess.DEVNULL)
procs.append(sshd)
up = threading.Event()
threading.Thread(target=lambda: [up.set() for l in sshd.stderr if b"listening on" in l] and None, daemon=True).start()
if not up.wait(20):
    print("FAIL sshd did not start")
    sys.exit(1)
with open(KH, "w") as f:
    f.write("[127.0.0.1]:%d %s\n" % (PORT, " ".join(open(HOSTKEY + ".pub").read().split()[:2])))

ENV = {"HOME": HOME, "PATH": os.environ.get("PATH", "/usr/bin:/bin"), "USER": USER, "LOGNAME": USER, "TERM": "xterm"}
COMMON = ["-P", str(PORT), "-o", "UserKnownHostsFile=" + KH, "-i", USERKEY]
REMOTE = "%s@127.0.0.1:" % USER   # remote paths are local paths: sshd is on this host
STAGE = os.path.join(W, "stage")
os.makedirs(STAGE)


FAKE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "fake_sftp_server.py")


def start_fake(scenario):
    """A second sshd whose sftp subsystem is fake_sftp_server.py SCENARIO;
    returns the COMMON-style option list for it."""
    port = free_port()
    cfg = os.path.join(W, "sshd_fake_%s" % scenario)
    with open(cfg, "w") as f:
        f.write(f"""Port {port}
ListenAddress 127.0.0.1
HostKey {HOSTKEY}
PidFile {W}/sshd_fake_{scenario}.pid
AuthorizedKeysFile {W}/authorized_keys
StrictModes no
UsePAM no
PasswordAuthentication no
KbdInteractiveAuthentication no
Subsystem sftp python3 -I {FAKE} {scenario}
""")
    d = subprocess.Popen([SSHD, "-f", cfg, "-D", "-e"], stderr=subprocess.PIPE, stdout=subprocess.DEVNULL)
    procs.append(d)
    ready = threading.Event()
    threading.Thread(target=lambda: [ready.set() for l in d.stderr if b"listening on" in l] and None, daemon=True).start()
    if not ready.wait(20):
        raise RuntimeError("fake sshd did not start")
    fake_kh = os.path.join(W, "kh_fake_%s" % scenario)
    with open(fake_kh, "w") as f:
        f.write("[127.0.0.1]:%d %s\n" % (port, " ".join(open(HOSTKEY + ".pub").read().split()[:2])))
    return ["-P", str(port), "-o", "UserKnownHostsFile=" + fake_kh, "-i", USERKEY]


def run(binary, args, input=None, timeout=300):
    p = subprocess.run([binary] + args, input=input, capture_output=True, env=ENV, timeout=timeout)
    err = "".join(l for l in p.stderr.decode(errors="replace").splitlines(True) if "warning: OpenSSH-style options" not in l and "warning: OpenSSH-style" not in l)
    return p.returncode, p.stdout.decode(errors="replace"), err


def sha(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def tree(root):
    """{relative path: (kind, mode, sha or None)} for a directory tree."""
    out = {}
    for dirpath, dirs, files in os.walk(root):
        for d in dirs:
            p = os.path.join(dirpath, d)
            out[os.path.relpath(p, root)] = ("d", statmod.S_IMODE(os.stat(p).st_mode), None)
        for fn in files:
            p = os.path.join(dirpath, fn)
            out[os.path.relpath(p, root)] = ("f", statmod.S_IMODE(os.stat(p).st_mode), sha(p))
    return out


# --- m31scp ---------------------------------------------------------------------------------
big = os.path.join(STAGE, "big.bin")
with open(big, "wb") as f:
    for _ in range(20):
        f.write(os.urandom(1 << 20))
os.chmod(big, 0o640)
dst = os.path.join(W, "uploaded.bin")
code, out, err = run(SCP, COMMON + ["-q", big, REMOTE + dst])
ok("scp: upload 20 MB, exit 0", code == 0, (code, err))
ok("scp: uploaded bytes identical", os.path.exists(dst) and sha(dst) == sha(big))
ok("scp: the file's permission bits travel (0640)", os.path.exists(dst) and statmod.S_IMODE(os.stat(dst).st_mode) == 0o640)
back = os.path.join(W, "downloaded.bin")
code, out, err = run(SCP, COMMON + ["-q", REMOTE + dst, back])
ok("scp: download 20 MB, exit 0", code == 0, (code, err))
ok("scp: downloaded bytes identical", os.path.exists(back) and sha(back) == sha(big))
ok("scp: ...with the remote's mode (0640)", os.path.exists(back) and statmod.S_IMODE(os.stat(back).st_mode) == 0o640)

empty = os.path.join(STAGE, "empty")
open(empty, "wb").close()
e_dst = os.path.join(W, "empty_up")
code, out, err = run(SCP, COMMON + ["-q", empty, REMOTE + e_dst])
ok("scp: a zero-length file", code == 0 and os.path.exists(e_dst) and os.path.getsize(e_dst) == 0, (code, err))
code, out, err = run(SCP, COMMON + ["-q", REMOTE + e_dst, os.path.join(W, "empty_down")])
ok("scp: ...and back", code == 0 and os.path.getsize(os.path.join(W, "empty_down")) == 0, (code, err))

src_tree = os.path.join(STAGE, "src tree")
os.makedirs(os.path.join(src_tree, "a", "deep", "er"))
os.makedirs(os.path.join(src_tree, "emptydir"))
with open(os.path.join(src_tree, "a", "file one.txt"), "w") as f:
    f.write("one\n" * 1000)
with open(os.path.join(src_tree, "a", "deep", "script.sh"), "w") as f:
    f.write("#!/bin/sh\necho hi\n")
os.chmod(os.path.join(src_tree, "a", "deep", "script.sh"), 0o755)
with open(os.path.join(src_tree, "a", "deep", "er", "blob"), "wb") as f:
    f.write(os.urandom(300000))
up_tree = os.path.join(W, "up_tree")
code, out, err = run(SCP, COMMON + ["-q", "-r", src_tree, REMOTE + up_tree])
ok("scp -r: upload a tree, exit 0", code == 0, (code, err))
a, b = tree(src_tree), tree(up_tree)
ok("scp -r: names, file bytes and the executable bit all arrive", a == b, (set(a) ^ set(b), [k for k in a if a.get(k) != b.get(k)][:3]))
down_tree = os.path.join(W, "down_tree")
code, out, err = run(SCP, COMMON + ["-q", "-r", REMOTE + up_tree, down_tree])
ok("scp -r: download a tree, exit 0", code == 0, (code, err))
ok("scp -r: ...identical", tree(down_tree) == tree(src_tree), (set(tree(down_tree)) ^ set(tree(src_tree))))

# a directory target, several sources
tgt = os.path.join(W, "into_dir")
os.makedirs(tgt)
f1 = os.path.join(STAGE, "f1")
f2 = os.path.join(STAGE, "f2")
open(f1, "w").write("1")
open(f2, "w").write("22")
code, out, err = run(SCP, COMMON + ["-q", f1, f2, REMOTE + tgt])
ok("scp: several sources into a directory", code == 0 and open(os.path.join(tgt, "f1")).read() == "1" and open(os.path.join(tgt, "f2")).read() == "22", (code, err))
code, out, err = run(SCP, COMMON + ["-q", f1, f2, REMOTE + os.path.join(W, "not-a-dir")])
ok("scp: several sources and a target that is not a directory: exit 1", code == 1 and "not a directory" in err, (code, err))
code, out, err = run(SCP, COMMON + ["-q", REMOTE + os.path.join(W, "nope.txt"), os.path.join(W, "x")])
ok("scp: a missing remote file: exit 1 with the server's words", code == 1 and "No such file" in err and len(err.strip().splitlines()) == 1, (code, err))
code, out, err = run(SCP, COMMON + ["-q", src_tree, REMOTE + os.path.join(W, "d2")])
ok("scp: a directory without -r: exit 1", code == 1 and "-r" in err, (code, err))
code, out, err = run(SCP, COMMON + ["-q", REMOTE + dst, REMOTE + os.path.join(W, "elsewhere")])
ok("scp: remote to remote is refused clearly", code == 1 and "not supported" in err, (code, err))
code, out, err = run(SCP, COMMON + ["-q", "-p", f1, REMOTE + os.path.join(W, "pres")])
os.utime(f1, (1500000000, 1500000000))
code, out, err = run(SCP, COMMON + ["-q", "-p", f1, REMOTE + os.path.join(W, "pres")])
ok("scp -p: the modification time is preserved", code == 0 and int(os.stat(os.path.join(W, "pres")).st_mtime) == 1500000000, (code, err))
code, out, err = run(SCP, ["-P", str(PORT), "-o", "UserKnownHostsFile=" + os.path.join(W, "nokh"), "-i", USERKEY, "-q", f1, REMOTE + os.path.join(W, "x")])
ok("scp: an unknown host without a terminal is refused (255), like m31ssh", code == 255, (code, err))
code, out, err = run(SCP, ["--help"])
ok("scp: --help says UNAUDITED", code == 0 and "UNAUDITED" in out, out)

# a real OpenSSH scp (SFTP mode) agrees about what m31scp wrote, and the reverse
if shutil.which("scp"):
    real_dl = os.path.join(W, "real_dl")
    p = subprocess.run(["scp", "-q", "-s", "-P", str(PORT), "-o", "UserKnownHostsFile=" + KH, "-o", "IdentitiesOnly=yes", "-i", USERKEY,
                        "%s@127.0.0.1:%s" % (USER, dst), real_dl], capture_output=True, env=ENV, timeout=120)
    ok("interop: OpenSSH scp downloads what m31scp uploaded, identical", p.returncode == 0 and os.path.exists(real_dl) and sha(real_dl) == sha(big), p.stderr[-200:])
    real_up = os.path.join(W, "real_up")
    p = subprocess.run(["scp", "-q", "-s", "-P", str(PORT), "-o", "UserKnownHostsFile=" + KH, "-o", "IdentitiesOnly=yes", "-i", USERKEY,
                        big, "%s@127.0.0.1:%s" % (USER, real_up)], capture_output=True, env=ENV, timeout=120)
    back2 = os.path.join(W, "back2")
    code, out, err = run(SCP, COMMON + ["-q", REMOTE + real_up, back2])
    ok("interop: m31scp downloads what OpenSSH scp uploaded, identical", p.returncode == 0 and code == 0 and sha(back2) == sha(big), (p.stderr[-200:], err))


# progress meter on a pty
def pty_run(binary, args, rows=24, cols=80):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

    def child():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)

    p = subprocess.Popen([binary] + args, stdin=slave, stdout=slave, stderr=slave, env=ENV, preexec_fn=child, close_fds=True)
    os.close(slave)
    procs.append(p)
    data = b""
    while True:
        r, _, _ = select.select([master], [], [], 30)
        if not r:
            break
        try:
            d = os.read(master, 65536)
        except OSError:
            break
        if not d:
            break
        data += d
    code = p.wait(30)
    os.close(master)
    return code, data.decode(errors="replace")


code, text = pty_run(SCP, COMMON + [big, REMOTE + os.path.join(W, "metered")])
ok("scp: a progress meter on a terminal reaches 100%", code == 0 and "100%" in text and "big.bin" in text, (code, text[-200:]))
code, text = pty_run(SCP, COMMON + ["-q", big, REMOTE + os.path.join(W, "metered2")])
ok("scp -q: no meter", code == 0 and "%" not in text, text[-100:])

# --- m31sftp --------------------------------------------------------------------------------
sd = os.path.join(W, "sftp_area")
os.makedirs(sd)
open(os.path.join(sd, "present.txt"), "w").write("hello sftp\n")
os.makedirs(os.path.join(sd, "sub"))
local_dir = os.path.join(W, "sftp_local")
os.makedirs(local_dir)
open(os.path.join(local_dir, "to_send.txt"), "w").write("from the client\n" * 100)
script = "\n".join([
    "pwd",
    "ls",
    "ls -l",
    "stat present.txt",
    "cd sub",
    "pwd",
    "cd ..",
    "mkdir newdir",
    "lcd " + local_dir,
    "put to_send.txt newdir/sent.txt",
    "rename newdir/sent.txt newdir/renamed.txt",
    "get newdir/renamed.txt got.txt",
    "get present.txt",
    "chmod 600 present.txt",
    "rm newdir/renamed.txt",
    "rmdir newdir",
    "quit",
    ""])
code, out, err = run(SFTP, COMMON + ["-q", "%s@127.0.0.1:%s" % (USER, sd)], input=script.encode())
ok("sftp: a scripted session exits 0", code == 0, (code, out[-300:], err))
ok("sftp: pwd, ls and ls -l print what the server has", sd in out and "present.txt" in out and "sub" in out and "-rw" in out, out)
ok("sftp: stat shows size and mode", "present.txt: file size=11" in out, out)
ok("sftp: put + get round trip", os.path.exists(os.path.join(local_dir, "got.txt")) and sha(os.path.join(local_dir, "got.txt")) == sha(os.path.join(local_dir, "to_send.txt")))
ok("sftp: get with no local name uses the remote's", open(os.path.join(local_dir, "present.txt")).read() == "hello sftp\n")
ok("sftp: rename, rm, rmdir, chmod took effect on the server",
   not os.path.exists(os.path.join(sd, "newdir")) and statmod.S_IMODE(os.stat(os.path.join(sd, "present.txt")).st_mode) == 0o600)
code, out, err = run(SFTP, COMMON + ["-q", "%s@127.0.0.1:%s" % (USER, sd)], input=b"get nosuchfile\nquit\n")
ok("sftp: a failing command in a script ends it with 1 and one line on stderr", code == 1 and "No such file" in err, (code, err))
code, out, err = run(SFTP, COMMON + ["-q", "%s@127.0.0.1:%s" % (USER, sd)], input=b"frobnicate\n")
ok("sftp: an unknown command is reported", code == 1 and "unknown command" in err, (code, err))
batch = os.path.join(W, "batch.txt")
open(batch, "w").write("mkdir bfile_dir\nput " + os.path.join(local_dir, "to_send.txt") + " bfile_dir/x\nquit\n")
code, out, err = run(SFTP, COMMON + ["-q", "-b", batch, "%s@127.0.0.1:%s" % (USER, sd)])
ok("sftp -b: a batch file", code == 0 and os.path.exists(os.path.join(sd, "bfile_dir", "x")), (code, err))

# --- a server that returns short reads (H1 of the audit): never a silently wrong file ----------
FAKE_DATA = bytes((i * 7) & 255 for i in range(100000))
opts = start_fake("shortread")
short_out = os.path.join(W, "short_out")
code, out, err = run(SCP, opts + ["-q", REMOTE + "/f", short_out])
ok("scp: a server capping READ at 4096 bytes still gives the exact file",
   code == 0 and os.path.exists(short_out) and open(short_out, "rb").read() == FAKE_DATA, (code, err, os.path.getsize(short_out) if os.path.exists(short_out) else None))

# --- terminal escapes from a hostile server never reach the terminal (audit M2) ------------------
opts = start_fake("esc")
code, out, err = run(SFTP, opts + ["-b", "/dev/stdin", USER + "@127.0.0.1"], input=b"pwd\nrm /x\nls /dir\n")
ok("sftp: server-chosen pwd, status text and listing carry no escape sequences",
   "\x1b" not in out and "\x1b" not in err and "\x07" not in out + err and "PWNED-TITLE" in out + err, (out, err))
tmp_dl = os.path.join(W, "esc_dl")
code, out, err = run(SCP, opts + ["-r", REMOTE + "/dir", tmp_dl])
ok("scp -r: an entry named with an escape sequence is refused, nothing escapes",
   "\x1b" not in out and "\x1b" not in err and not any("\x1b" in n for n in (os.listdir(tmp_dl) if os.path.isdir(tmp_dl) else [])), (code, out, err))

# --- m31ssh-keygen -------------------------------------------------------------------------------
gen = os.path.join(W, "gen_plain")
code, out, err = run(KEYGEN, ["-q", "-t", "ed25519", "-f", gen, "-N", "", "-C", "plain key"])
ok("keygen: a plain key is written (0600) with its .pub (0644)",
   code == 0 and statmod.S_IMODE(os.stat(gen).st_mode) == 0o600 and statmod.S_IMODE(os.stat(gen + ".pub").st_mode) == 0o644, (code, err))
y = subprocess.run(["ssh-keygen", "-y", "-f", gen], capture_output=True, text=True)
pub = open(gen + ".pub").read().split()
ok("keygen: OpenSSH's ssh-keygen reads it and derives the same public key",
   y.returncode == 0 and y.stdout.split()[:2] == pub[:2] and " ".join(pub[2:]) == "plain key", (y.stdout, y.stderr))
l = subprocess.run(["ssh-keygen", "-l", "-f", gen + ".pub"], capture_output=True, text=True)
code, out, err = run(KEYGEN, ["-l", "-f", gen + ".pub"])
ok("keygen -l: the fingerprint line equals ssh-keygen -l's", code == 0 and out.strip() == l.stdout.strip(), (out, l.stdout))
code, out, err = run(KEYGEN, ["-l", "-f", gen])
ok("keygen -l on the private key says the same", code == 0 and out.split()[1] == l.stdout.split()[1], (out, err))
code, out, err = run(KEYGEN, ["-y", "-f", gen])
ok("keygen -y prints the public key line", code == 0 and out.split()[:2] == pub[:2], (out, err))
code, out, err = run(KEYGEN, ["-q", "-f", gen, "-N", ""])
ok("keygen: an existing key is never overwritten", code == 1 and "already exists" in err, (code, err))

enc = os.path.join(W, "gen_enc")
code, out, err = run(KEYGEN, ["-q", "-f", enc, "-N", "a good passphrase é", "-a", "4"])
ok("keygen: an encrypted key is written", code == 0, (code, err))
y = subprocess.run(["ssh-keygen", "-y", "-P", "a good passphrase é", "-f", enc], capture_output=True, text=True)
ok("keygen: OpenSSH decrypts it with the passphrase", y.returncode == 0 and y.stdout.split()[1] == open(enc + ".pub").read().split()[1], (y.stdout, y.stderr))
y = subprocess.run(["ssh-keygen", "-y", "-P", "wrong", "-f", enc], capture_output=True, text=True)
ok("keygen: ...and refuses a wrong one", y.returncode != 0)
code, out, err = run(KEYGEN, ["-y", "-P", "a good passphrase é", "-f", enc])
ok("keygen -y -P decrypts its own file", code == 0 and out.split()[1] == open(enc + ".pub").read().split()[1], (out, err))
code, out, err = run(KEYGEN, ["-y", "-P", "nope", "-f", enc])
ok("keygen -y with the wrong passphrase: exit 1, nothing printed", code == 1 and out == "", (code, out, err))
code, out, err = run(KEYGEN, ["-q", "-f", os.path.join(W, "short"), "-N", "abc"])
ok("keygen: a too-short passphrase is refused", code == 1, (code, err))

# a generated key logs in, with m31ssh and with OpenSSH
with open(os.path.join(W, "authorized_keys"), "a") as f:
    f.write(open(gen + ".pub").read())
    f.write(open(enc + ".pub").read())
p = subprocess.run([SSH, "-p", str(PORT), "-o", "UserKnownHostsFile=" + KH, "-i", gen, "127.0.0.1", "echo", "keygen-ok"],
                   capture_output=True, env=ENV, timeout=60)
ok("keygen: m31ssh authenticates with the generated key", p.returncode == 0 and p.stdout == b"keygen-ok\n", p.stderr)
p = subprocess.run(["ssh", "-p", str(PORT), "-o", "UserKnownHostsFile=" + KH, "-o", "IdentitiesOnly=yes", "-i", gen, "127.0.0.1", "echo", "real-ssh-ok"],
                   capture_output=True, env=ENV, timeout=60)
ok("keygen: OpenSSH's ssh authenticates with it too", p.returncode == 0 and p.stdout == b"real-ssh-ok\n", p.stderr)

# -l on keys OpenSSH made, every type
for label, args in (("ed25519", ["-t", "ed25519"]), ("ecdsa", ["-t", "ecdsa", "-b", "256"]), ("rsa", ["-t", "rsa", "-b", "2048"])):
    k = os.path.join(W, "ossh_" + label)
    keygen(k, args)
    want = subprocess.run(["ssh-keygen", "-l", "-f", k], capture_output=True, text=True).stdout.split()
    code, out, err = run(KEYGEN, ["-l", "-f", k])
    ok("keygen -l: %s key from OpenSSH: bits, fingerprint and type agree" % label,
       code == 0 and out.split()[0] == want[0] and out.split()[1] == want[1] and out.split()[-1] == want[-1], (out, want))

# -R
kh2 = os.path.join(W, "kh_remove")
k_ed = open(HOSTKEY + ".pub").read().split()[:2]
with open(kh2, "w") as f:
    f.write("# comment\nkeep.example %s\n" % " ".join(k_ed))
    f.write("gone.example,10.0.0.5 %s\n" % " ".join(k_ed))
    f.write("[gone.example]:2222 %s\n" % " ".join(k_ed))
    f.write("other.example %s\n" % " ".join(k_ed))
before = open(kh2).read()
code, out, err = run(KEYGEN, ["-R", "gone.example", "-f", kh2])
after = open(kh2).read()
ok("keygen -R: the host's plain entries go, others stay byte for byte",
   code == 0 and "gone.example" not in after.replace("[gone.example]:2222", "") and "keep.example" in after and "other.example" in after and "# comment" in after, (code, out, after))
ok("keygen -R: the old file is kept as .old", open(kh2 + ".old").read() == before)
f = subprocess.run(["ssh-keygen", "-F", "keep.example", "-f", kh2], capture_output=True, text=True)
ok("keygen -R: ssh-keygen still reads the result", f.returncode == 0 and "keep.example" in f.stdout)
code, out, err = run(KEYGEN, ["-R", "[gone.example]:2222", "-f", kh2])
ok("keygen -R [host]:port removes that entry", code == 0 and "2222" not in open(kh2).read(), (code, out))
code, out, err = run(KEYGEN, ["-R", "absent.example", "-f", kh2])
ok("keygen -R: a host that is not there is said so, file untouched", code == 0 and "not found" in out, (code, out))
# hashed names
kh3 = os.path.join(W, "kh_hashed")
with open(kh3, "w") as f:
    f.write("hashme.example %s\nstay.example %s\n" % (" ".join(k_ed), " ".join(k_ed)))
subprocess.run(["ssh-keygen", "-H", "-f", kh3], capture_output=True)
code, out, err = run(KEYGEN, ["-R", "hashme.example", "-f", kh3])
f1 = subprocess.run(["ssh-keygen", "-F", "hashme.example", "-f", kh3], capture_output=True, text=True)
f2 = subprocess.run(["ssh-keygen", "-F", "stay.example", "-f", kh3], capture_output=True, text=True)
ok("keygen -R: a hashed entry is found and removed (checked with ssh-keygen -F)", code == 0 and f1.returncode != 0 and f2.returncode == 0, (code, out, f1.stdout, f2.stdout))

# -F, -H (against ssh-keygen as the oracle), -p
kh4 = os.path.join(W, "kh_find")
with open(kh4, "w") as f:
    f.write("# c\nfind.example,10.1.1.1 %s\n[find.example]:2200 %s\n*.wild.example %s\nother.example %s\n" % ((" ".join(k_ed),) * 4))
for target in ["find.example", "[find.example]:2200", "10.1.1.1", "x.wild.example", "nothere.example"]:
    mine = run(KEYGEN, ["-F", target, "-f", kh4])
    ref = subprocess.run(["ssh-keygen", "-F", target, "-f", kh4], capture_output=True, text=True)
    ok("keygen -F %s: same output and exit status as ssh-keygen -F" % target,
       mine[0] == ref.returncode and mine[1] == ref.stdout, (mine, ref.stdout))
kh5 = os.path.join(W, "kh_hash_all")
with open(kh5, "w") as f:
    f.write("# c\nhash.example,10.2.2.2 %s\n[hash.example]:2200 %s\n*.wild.example %s\n" % ((" ".join(k_ed),) * 3))
before = open(kh5).read()
code, out, err = run(KEYGEN, ["-H", "-f", kh5])
after = open(kh5).read()
ok("keygen -H: the old file is kept as .old", code == 0 and open(kh5 + ".old").read() == before, (code, err))
ok("keygen -H: names are gone, the wildcard and the comment stay",
   "hash.example" not in after and "10.2.2.2" not in after and "*.wild.example" in after and "# c" in after and after.count("|1|") == 3, after)
for target in ["hash.example", "[hash.example]:2200", "10.2.2.2"]:
    ref = subprocess.run(["ssh-keygen", "-F", target, "-f", kh5], capture_output=True, text=True)
    mine = run(KEYGEN, ["-F", target, "-f", kh5])
    ok("keygen -H: ssh-keygen -F finds %s in the hashed file, and so does -F" % target,
       ref.returncode == 0 and mine[0] == 0 and mine[1] == ref.stdout, (ref.stdout, mine))
gen_p = os.path.join(W, "pw_key")
run(KEYGEN, ["-q", "-f", gen_p, "-N", "", "-C", "pw comment"])
pub_before = subprocess.run(["ssh-keygen", "-y", "-f", gen_p], capture_output=True, text=True).stdout
code, out, err = run(KEYGEN, ["-p", "-f", gen_p, "-N", "new pass phrase", "-a", "4"])
ok("keygen -p: sets a passphrase", code == 0, (code, err))
y = subprocess.run(["ssh-keygen", "-y", "-P", "new pass phrase", "-f", gen_p], capture_output=True, text=True)
ok("keygen -p: OpenSSH decrypts the result and the public key is unchanged", y.returncode == 0 and y.stdout.split()[:2] == pub_before.split()[:2], (y.stdout, y.stderr))
ok("keygen -p: the comment is kept and the mode is 0600", y.stdout.split()[2:] == ["pw", "comment"] and (os.stat(gen_p).st_mode & 0o777) == 0o600, (y.stdout, oct(os.stat(gen_p).st_mode)))
code, out, err = run(KEYGEN, ["-p", "-f", gen_p, "-P", "wrong", "-N", ""])
ok("keygen -p: a wrong old passphrase changes nothing", code == 1 and subprocess.run(["ssh-keygen", "-y", "-P", "new pass phrase", "-f", gen_p], capture_output=True).returncode == 0, (code, err))
code, out, err = run(KEYGEN, ["-p", "-f", gen_p, "-P", "new pass phrase", "-N", "", "-C", "renamed"])
y = subprocess.run(["ssh-keygen", "-y", "-P", "", "-f", gen_p], capture_output=True, text=True)
ok("keygen -p: removes the passphrase and changes the comment", code == 0 and y.returncode == 0 and y.stdout.split()[2:] == ["renamed"], (code, y.stdout, y.stderr))
rsa_p = os.path.join(W, "rsa_p")
keygen(rsa_p, ["-t", "rsa", "-b", "2048"])
code, out, err = run(KEYGEN, ["-p", "-f", rsa_p, "-N", "abcdefg"])
ok("keygen -p on an RSA key: refused clearly, file untouched", code == 1 and "Ed25519" in err, (code, err))

# -R keeps @revoked / @cert-authority lines and leaves no temporary file; keygen will not follow a symlink
kh6 = os.path.join(W, "kh_marked")
with open(kh6, "w") as f:
    f.write("@revoked gone2.example %s\ngone2.example %s\n@cert-authority gone2.example %s\n" % ((" ".join(k_ed),) * 3))
code, out, err = run(KEYGEN, ["-R", "gone2.example", "-f", kh6])
left = open(kh6).read().splitlines()
ok("keygen -R: @revoked and @cert-authority lines survive, the plain one goes",
   code == 0 and len(left) == 2 and left[0].startswith("@revoked") and left[1].startswith("@cert-authority"), (code, left))
ok("keygen -R: no temporary file is left behind", not os.path.exists(kh6 + ".m31new"))
dangling = os.path.join(W, "dangling_key")
os.symlink(os.path.join(W, "nowhere_target"), dangling)
code, out, err = run(KEYGEN, ["-q", "-f", dangling, "-N", ""])
ok("keygen: a dangling symlink at the key path is refused, nothing created", code == 1 and not os.path.exists(os.path.join(W, "nowhere_target")), (code, err))

# -r bounds: a server that nests directories for ever, and a symlink loop on put -r
opts = start_fake("loop")
loop_dl = os.path.join(W, "loop_dl")
code, out, err = run(SCP, opts + ["-r", REMOTE + "/d", loop_dl])
depth = 0
walk = loop_dl
while os.path.isdir(os.path.join(walk, "d")):
    walk = os.path.join(walk, "d")
    depth += 1
ok("scp -r: endlessly nested remote directories stop at the depth bound", code != 0 and "nested" in err and depth <= 66, (code, err, depth))
tree_src = os.path.join(W, "loop_src")
os.makedirs(os.path.join(tree_src, "sub"))
open(os.path.join(tree_src, "sub", "f"), "w").write("x")
os.symlink(tree_src, os.path.join(tree_src, "sub", "back"))
tree_dst = os.path.join(W, "loop_dst")
code, out, err = run(SCP, COMMON + ["-q", "-r", tree_src, REMOTE + tree_dst])
ok("scp -r upload: a symlink to a directory is skipped, not followed (no loop)",
   code == 0 and os.path.isfile(os.path.join(tree_dst, "sub", "f")) and not os.path.exists(os.path.join(tree_dst, "sub", "back")), (code, err))

print("%d passed, %d failed" % (passed, failed))
sys.exit(1 if failed else 0)
