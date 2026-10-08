#!/usr/bin/env python3
"""Optional live check: a real OpenSSL server's TLS 1.3 records through tls13_record.

  live_tls13.py <compiled t_tls13_live> [workdir]

For each of the three TLS 1.3 suites, starts `openssl s_server -tls1_3
-ciphersuites <that suite> -keylogfile ... -www`, connects Python's ssl client through a recording proxy,
then gives the server-to-client bytes and the server's traffic secrets (from
the key log) to the m31 program and compares what it decrypted with what the
client received. Exit 0 = every suite that could run matched (at least one), 1 = a mismatch,
2 = nothing could run (no openssl, a timeout, ...): the caller treats 2 as a
skip.
"""
import hashlib
import os
import socket
import ssl
import subprocess
import sys
import tempfile
import threading
import time

TIMEOUT = 20
SUITES = ["TLS_CHACHA20_POLY1305_SHA256", "TLS_AES_128_GCM_SHA256", "TLS_AES_256_GCM_SHA384"]


class Skip(Exception):
    pass


def skip(reason):
    raise Skip(reason)


def free_port():
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def main(suite):
    program = sys.argv[1]
    work = tempfile.mkdtemp(prefix="tls13_live_")
    key, cert, keylog = (os.path.join(work, n) for n in ("key.pem", "cert.pem", "keylog"))
    open(keylog, "w").close()
    try:
        subprocess.run(
            ["openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1",
             "-nodes", "-keyout", key, "-out", cert, "-subj", "/CN=localhost", "-days", "1"],
            check=True, capture_output=True, timeout=TIMEOUT)
    except (OSError, subprocess.SubprocessError) as error:
        skip("cannot make a certificate: %s" % error)
    server_port = free_port()
    server = subprocess.Popen(
        ["openssl", "s_server", "-accept", "127.0.0.1:%d" % server_port, "-tls1_3",
         "-ciphersuites", suite, "-cert", cert, "-key", key,
         "-keylogfile", keylog, "-www", "-num_tickets", "2"],
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        deadline = time.time() + TIMEOUT
        while True:
            try:
                socket.create_connection(("127.0.0.1", server_port), timeout=1).close()
                break
            except OSError:
                if time.time() > deadline or server.poll() is not None:
                    skip("openssl s_server did not start")
                time.sleep(0.1)
        # The probe connection above made s_server serve one (failed) session;
        # -www keeps accepting, so the real one follows.
        proxy = socket.socket()
        proxy.bind(("127.0.0.1", 0))
        proxy.listen(1)
        proxy.settimeout(TIMEOUT)
        captured = bytearray()

        def relay():
            try:
                client_side, _ = proxy.accept()
                server_side = socket.create_connection(("127.0.0.1", server_port), timeout=TIMEOUT)
            except OSError:
                return

            def upstream():
                try:
                    while True:
                        data = client_side.recv(65536)
                        if not data:
                            break
                        server_side.sendall(data)
                except OSError:
                    pass
                try:
                    server_side.shutdown(socket.SHUT_WR)
                except OSError:
                    pass

            threading.Thread(target=upstream, daemon=True).start()
            try:
                while True:
                    data = server_side.recv(65536)
                    if not data:
                        break
                    captured.extend(data)
                    client_side.sendall(data)
            except OSError:
                pass
            try:
                client_side.shutdown(socket.SHUT_WR)
            except OSError:
                pass

        relayer = threading.Thread(target=relay, daemon=True)
        relayer.start()
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        context.check_hostname = False
        context.verify_mode = ssl.CERT_NONE
        context.minimum_version = ssl.TLSVersion.TLSv1_3
        try:
            raw = socket.create_connection(("127.0.0.1", proxy.getsockname()[1]), timeout=TIMEOUT)
            client = context.wrap_socket(raw, server_hostname="localhost")
            cipher = client.cipher()[0]
            client.sendall(b"GET / HTTP/1.0\r\n\r\n")
            body = b""
            while True:
                chunk = client.recv(65536)
                if not chunk:
                    break
                body += chunk
            client.close()
        except (OSError, ssl.SSLError) as error:
            skip("client could not talk to s_server: %s" % error)
        relayer.join(TIMEOUT)
        if cipher != suite:
            skip("negotiated %s, not %s" % (cipher, suite))
        secrets = {}
        with open(keylog) as handle:
            for line in handle:
                parts = line.split()
                if len(parts) == 3:
                    secrets[parts[0]] = parts[2]
        needed = ("SERVER_HANDSHAKE_TRAFFIC_SECRET", "SERVER_TRAFFIC_SECRET_0")
        if not all(name in secrets for name in needed):
            skip("the key log has no server traffic secrets")
    finally:
        server.kill()
        server.wait()

    result = subprocess.run(
        [program, bytes(captured).hex(), secrets[needed[0]], secrets[needed[1]], suite],
        capture_output=True, text=True, timeout=TIMEOUT)
    lines = result.stdout.splitlines()
    expected_types = [2, 8, 11, 15, 20]
    seen_types = [int(line.split("type=")[1].split()[0]) for line in lines if line.startswith("handshake")]
    want = "application size=%d sha256=%s" % (len(body), hashlib.sha256(body).hexdigest())
    problems = []
    if result.returncode != 0:
        problems.append("t_tls13_live exited %d: %s" % (result.returncode, result.stderr.strip()[-200:]))
    if seen_types != expected_types:
        problems.append("handshake message types %s, wanted %s" % (seen_types, expected_types))
    if want not in lines:
        problems.append("application data differs from what the client received (%s)" % want)
    if "close_notify true" not in lines:
        problems.append("no close_notify")
    if problems:
        print("\n".join(lines))
        print("MISMATCH %s: %s" % (suite, "; ".join(problems)))
        sys.exit(1)
    return "%s: %d handshake messages, %d octets of application data" % (suite, len(seen_types), len(body))


done = []
skipped = []
for suite_name in SUITES:
    try:
        done.append(main(suite_name))
    except Skip as reason:
        skipped.append("%s (%s)" % (suite_name, reason))
if not done:
    print("skip: " + "; ".join(skipped))
    sys.exit(2)
print("matched %d suites, all with close_notify: %s%s" % (len(done), "; ".join(done), ("; skipped " + ", ".join(skipped)) if skipped else ""))
