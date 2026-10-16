#!/usr/bin/env python3
"""End-to-end SSH numbers: m31ssh / m31scp against a disposable local OpenSSH
sshd, next to the real `ssh` / `scp` with the same cipher.

    python3 bench/crypto/e2e.py                    # both ciphers, 32 MiB, best of 3
    python3 bench/crypto/e2e.py --mib 64 --reps 5 --tag after --out e2e.json

Everything is disposable: temp host and user keys, an `sshd -D` on an ephemeral
127.0.0.1 port, a temp HOME, no fixed /tmp paths. Every process started is killed
by pid on exit. Measures, per cipher (chacha20-poly1305@openssh.com and
aes256-gcm@openssh.com, which the server is restricted to so the client has no
choice):

  download   `<client> host cat file > /dev/null`            (server -> client, MiB/s)
  upload     `<client> file host:dest`                       (client -> server, MiB/s;
             real scp vs m31scp)
  handshake  `<client> host true`: connect + KEX + auth + exec + exit, ms

Needs sshd, ssh, scp, ssh-keygen and a built compiler (`cargo build`).
"""
import argparse, json, os, platform, shutil, socket, statistics, subprocess, sys, tempfile, time

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
CIPHERS = ["chacha20-poly1305@openssh.com", "aes256-gcm@openssh.com"]


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def build(app, out):
    r = subprocess.run(["bash", os.path.join(ROOT, "apps", "build.sh"), app, "-o", out], capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit("build %s failed:\n%s%s" % (app, r.stdout, r.stderr))


def timed(cmd, **kw):
    t0 = time.perf_counter()
    r = subprocess.run(cmd, **kw)
    return time.perf_counter() - t0, r


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--mib", type=int, default=32)
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--handshakes", type=int, default=10)
    ap.add_argument("--tag", default="run")
    ap.add_argument("--out")
    ap.add_argument("--pin", type=int, help="cpu for the clients (taskset); sshd is unpinned")
    a = ap.parse_args()
    for tool in ("sshd", "ssh", "scp", "ssh-keygen"):
        if not shutil.which(tool):
            sys.exit("%s not found" % tool)
    sshd_bin = shutil.which("sshd")
    pre = ["taskset", "-c", str(a.pin)] if a.pin is not None and shutil.which("taskset") else []
    work = tempfile.mkdtemp(prefix="m31e2e-")
    procs = []
    try:
        home = os.path.join(work, "home")
        os.makedirs(os.path.join(home, ".ssh"), mode=0o700)
        user = os.path.join(home, ".ssh", "id_ed25519")
        hostkey = os.path.join(work, "host_key")
        for k in (user, hostkey):
            subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", k], check=True)
        shutil.copy(user + ".pub", os.path.join(work, "authorized_keys"))
        bins = {}
        for app in ("m31ssh", "m31scp"):
            bins[app] = os.path.join(work, app)
            build(app, bins[app])
        data = os.path.join(work, "payload")
        with open(data, "wb") as f:
            blk = os.urandom(1 << 20)
            for _ in range(a.mib):
                f.write(blk)
        kh = os.path.join(work, "known_hosts")
        env = {"HOME": home, "PATH": os.environ.get("PATH", "/usr/bin:/bin"), "USER": os.environ.get("USER", "u"),
               "LOGNAME": os.environ.get("USER", "u"), "TERM": "xterm"}
        results = {}
        for cipher in CIPHERS:
            port = free_port()
            cfg = os.path.join(work, "sshd_config_%d" % port)
            open(cfg, "w").write("""Port %d
ListenAddress 127.0.0.1
HostKey %s
PidFile %s/sshd_%d.pid
AuthorizedKeysFile %s/authorized_keys
StrictModes no
UsePAM no
KexAlgorithms curve25519-sha256
HostKeyAlgorithms ssh-ed25519
Ciphers %s
Subsystem sftp internal-sftp
PubkeyAuthentication yes
PasswordAuthentication no
KbdInteractiveAuthentication no
PrintMotd no
PrintLastLog no
""" % (port, hostkey, work, port, work, cipher))
            sshd = subprocess.Popen([sshd_bin, "-f", cfg, "-D", "-e"], stderr=subprocess.DEVNULL, stdout=subprocess.DEVNULL)
            procs.append(sshd)
            for _ in range(100):
                try:
                    socket.create_connection(("127.0.0.1", port), 0.2).close()
                    break
                except OSError:
                    time.sleep(0.1)
            else:
                sys.exit("sshd did not start")
            opts = ["-o", "UserKnownHostsFile=" + kh, "-o", "StrictHostKeyChecking=accept-new", "-i", user]
            real_ssh = ["ssh", "-p", str(port), "-o", "Ciphers=" + cipher, "-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes"] + opts + ["127.0.0.1"]
            m31_ssh = [bins["m31ssh"], "-p", str(port)] + opts + ["127.0.0.1"]
            real_scp = ["scp", "-q", "-P", str(port), "-o", "Ciphers=" + cipher, "-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes"] + opts
            m31_scp = [bins["m31scp"], "-q", "-P", str(port)] + opts
            # accept the host key once, for both clients' known_hosts files
            subprocess.run(pre + real_ssh + ["true"], env=env, check=True, capture_output=True)
            subprocess.run(pre + m31_ssh + ["true"], env=env, check=True, capture_output=True)
            res = {}
            for label, ssh_cmd, scp_cmd in (("openssh", real_ssh, real_scp), ("m31", m31_ssh, m31_scp)):
                dl, ul, hs = [], [], []
                for _ in range(a.reps):
                    t, r = timed(pre + ssh_cmd + ["cat", data], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
                    if r.returncode != 0:
                        sys.exit("%s download failed: %s" % (label, r.stderr[-300:]))
                    dl.append(a.mib / t)
                    dest = os.path.join(work, "upload_%s" % label)
                    t, r = timed(pre + scp_cmd + [data, "127.0.0.1:" + dest], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
                    if r.returncode != 0 or os.path.getsize(dest) != a.mib << 20:
                        sys.exit("%s upload failed: %s" % (label, r.stderr[-300:]))
                    ul.append(a.mib / t)
                    os.unlink(dest)
                for _ in range(a.handshakes):
                    t, r = timed(pre + ssh_cmd + ["true"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                    hs.append(t * 1000)
                res[label] = {"download_mibs": max(dl), "upload_mibs": max(ul), "handshake_ms_min": min(hs),
                              "handshake_ms_median": statistics.median(hs)}
                print("%-30s %-8s download %8.1f MiB/s  upload %8.1f MiB/s  handshake min %7.1f ms median %7.1f ms" %
                      (cipher, label, res[label]["download_mibs"], res[label]["upload_mibs"],
                       res[label]["handshake_ms_min"], res[label]["handshake_ms_median"]), flush=True)
            results[cipher] = res
            sshd.kill()
            sshd.wait()
        doc = {"tag": a.tag, "mib": a.mib, "reps": a.reps, "results": results, "os": platform.platform(),
               "load": os.getloadavg()}
        if a.out:
            json.dump(doc, open(a.out, "w"), indent=1)
        print("\n| cipher | client | download MiB/s | upload MiB/s | handshake ms (min / median) |")
        print("|---|---|---:|---:|---:|")
        for cipher, res in results.items():
            for label, r in res.items():
                print("| %s | %s | %.1f | %.1f | %.0f / %.0f |" % (cipher, label, r["download_mibs"], r["upload_mibs"],
                                                                   r["handshake_ms_min"], r["handshake_ms_median"]))
    finally:
        for p in procs:
            try:
                p.kill()
                p.wait(timeout=5)
            except Exception:
                pass
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
