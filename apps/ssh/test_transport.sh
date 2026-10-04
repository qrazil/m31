#!/usr/bin/env bash
# `docs/ssh-decision.md` Phase 2's other half: the live-handshake oracle.
# `test.sh`'s own checks are all pure functions over fixed byte strings --
# this is the one file that proves `lib/ssh.m31`'s `ssh.connect` actually
# interoperates with a real, independent SSH implementation, per
# `docs/ssh-decision.md` §4 ("the oracle is a real implementation, not this
# project's own code checked against itself"): a disposable local `sshd`
# (OpenSSH), a generated ed25519 host key, torn down after the test.
#
#   bash apps/ssh/test_transport.sh
#
# Needs `sshd`, `ssh-keygen` and `python3` on `PATH` (the last only to pull
# the raw 32-octet public key out of `ssh-keygen`'s OpenSSH-format `.pub`
# file -- no cryptography package needed here, unlike `test.sh`).
#
# `sshd` is started with `-D` (foreground): it daemonizes by default, which
# would orphan it from the ordinary `$!`/`kill`/`wait` teardown idiom every
# other test script in this project already uses (`apps/httpserver/test.sh`)
# -- `-D` is the one deviation needed to keep that idiom working for `sshd`
# specifically. `KexAlgorithms`/`HostKeyAlgorithms`/`Ciphers` are pinned to
# exactly this client's one suite, confirmed against this host's real `sshd`
# to override its system-wide crypto-policy cleanly (Fedora: `update-crypto-
# policies`), so the negotiation this test runs is a genuine "does a real
# `sshd` accept this exact handshake," not one that happens to also offer a
# fallback this client would never try.
set -uo pipefail
cd "$(dirname "$0")/../.."
. ./runtime/arch.sh

command -v sshd >/dev/null || { echo "sshd not found on PATH -- skipping"; exit 0; }
command -v ssh-keygen >/dev/null || { echo "ssh-keygen not found on PATH -- skipping"; exit 0; }
command -v python3 >/dev/null || { echo "python3 not found on PATH -- skipping"; exit 0; }
SSHD_BIN=$(command -v sshd)

LANGC=./target/debug/m31c
WORK=$(mktemp -d)
pass=0
fail=0
server_pid=""
trunc_pid=""

note() { printf '\033[32mok\033[0m   %s\n' "$1"; pass=$((pass + 1)); }
bad()  { printf '\033[31mFAIL\033[0m %s\n' "$1"; shift; printf '%s\n' "$@" | sed 's/^/     /'; fail=$((fail + 1)); }

cleanup() {
    [ -n "$server_pid" ] && kill "$server_pid" >/dev/null 2>&1
    [ -n "$server_pid" ] && wait "$server_pid" 2>/dev/null
    [ -n "$trunc_pid" ] && kill "$trunc_pid" >/dev/null 2>&1
    [ -n "$trunc_pid" ] && wait "$trunc_pid" 2>/dev/null
    rm -rf "$WORK"
}
trap cleanup EXIT

# --- a disposable sshd, restricted to exactly this client's one suite -------

ssh-keygen -t ed25519 -f "$WORK/host_key" -N "" -q
ssh-keygen -t ed25519 -f "$WORK/other_key" -N "" -q

HOST_KEY_HEX=$(python3 -c "
import base64
line = open('$WORK/host_key.pub').read().split()
blob = base64.b64decode(line[1])
n = int.from_bytes(blob[0:4], 'big')
k0 = 4 + n
kn = int.from_bytes(blob[k0:k0+4], 'big')
print(blob[k0+4:k0+4+kn].hex())
")
OTHER_KEY_HEX=$(python3 -c "
import base64
line = open('$WORK/other_key.pub').read().split()
blob = base64.b64decode(line[1])
n = int.from_bytes(blob[0:4], 'big')
k0 = 4 + n
kn = int.from_bytes(blob[k0:k0+4], 'big')
print(blob[k0+4:k0+4+kn].hex())
")

PORT=$((20000 + (RANDOM % 20000)))

cat > "$WORK/sshd_config" <<EOF
Port $PORT
ListenAddress 127.0.0.1
HostKey $WORK/host_key
PidFile $WORK/sshd.pid
KexAlgorithms curve25519-sha256
HostKeyAlgorithms ssh-ed25519
Ciphers chacha20-poly1305@openssh.com
PubkeyAuthentication yes
PasswordAuthentication no
EOF

"$SSHD_BIN" -f "$WORK/sshd_config" -D -e >"$WORK/sshd.log" 2>&1 &
server_pid=$!

up=0
tries=0
while [ "$tries" -lt 100 ]; do
    if (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then
        exec 3>&-
        up=1
        break
    fi
    sleep 0.05
    tries=$((tries + 1))
done

if [ "$up" != 1 ]; then
    bad "sshd did not come up" "$(cat "$WORK/sshd.log")"
    exit 1
fi
note "disposable sshd listening on 127.0.0.1:$PORT (pid $server_pid)"

# --- a stub listener that accepts and immediately closes, no banner ever ---
# sent -- proves a truncated connection fails cleanly (Error.Io), not a hang,
# per `docs/ssh-decision.md` §4's test plan. A real `sshd` killed mid-
# handshake would prove the identical thing less deterministically (a race
# on exactly when the kill lands); this is the same failure mode without
# the race.

TRUNC_PORT=$((PORT + 1))
python3 -c "
import socket
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(('127.0.0.1', $TRUNC_PORT))
s.listen(5)
# Loops rather than accepting once: the readiness probe below makes its own
# connection before the real test does, and both must be served.
for _ in range(5):
    conn, _ = s.accept()
    conn.close()
s.close()
" &
trunc_pid=$!

up=0
tries=0
while [ "$tries" -lt 100 ]; do
    if (exec 3<>"/dev/tcp/127.0.0.1/$TRUNC_PORT") 2>/dev/null; then
        exec 3>&-
        up=1
        break
    fi
    sleep 0.05
    tries=$((tries + 1))
done
if [ "$up" != 1 ]; then
    bad "stub truncating listener did not come up"
fi

# --- the m31 client program, against both the real and a wrong host key ----

cat > "$WORK/t_transport.m31" <<EOF
import ssh;

bytes hex_decode(str s) {
    bytes out = [];
    int i = 0;
    while (i + 1 < s.size()) {
        out.push(hex_val(s.byte_at(i)) << 4 | hex_val(s.byte_at(i + 1)));
        i = i + 2;
    }
    return out;
}

int hex_val(int c) {
    if (c >= '0' && c <= '9') {
        return c - '0';
    }
    if (c >= 'a' && c <= 'f') {
        return c - 'a' + 10;
    }
    trap("hex_decode: bad hex digit");
}

bytes good_key = hex_decode("$HOST_KEY_HEX");
bytes wrong_key = hex_decode("$OTHER_KEY_HEX");
int port = $PORT;
int trunc_port = $TRUNC_PORT;

match (ssh.connect("127.0.0.1", port, good_key)) {
    case Ok(ssh.Transport t): {
        print("handshake ok");
        bytes req = [];
        req.push(ssh.SSH_MSG_SERVICE_REQUEST);
        ssh.push_string(req, "ssh-userauth".to_bytes());
        match (t.send(req)) {
            case Ok(int n): {
            }
            case Err(ssh.Error e): {
                trap("send SERVICE_REQUEST: " + e.to_str());
            }
        }
        match (t.recv()) {
            case Ok(bytes resp): {
                if (resp.size() >= 1 && resp[0] == ssh.SSH_MSG_SERVICE_ACCEPT) {
                    print("service-accept ok");
                } else {
                    trap("unexpected response, first byte " + resp[0].to_str());
                }
            }
            case Err(ssh.Error e): {
                trap("recv SERVICE_ACCEPT: " + e.to_str());
            }
        }
        match (t.close()) {
            case Ok(bool ok): {
            }
            case Err(ssh.Error e): {
            }
        }
    }
    case Err(ssh.Error e): {
        trap("handshake with the real host key failed: " + e.to_str());
    }
}

match (ssh.connect("127.0.0.1", port, wrong_key)) {
    case Ok(ssh.Transport t): {
        trap("handshake with the WRONG host key unexpectedly succeeded");
    }
    case Err(ssh.Error e): {
        print("wrong-host-key rejected: " + e.to_str());
    }
}

match (ssh.connect("127.0.0.1", trunc_port, good_key)) {
    case Ok(ssh.Transport t): {
        trap("handshake against a connection closed with no banner unexpectedly succeeded");
    }
    case Err(ssh.Error e): {
        print("truncated connection rejected: " + e.to_str());
    }
}
EOF

if ! "$LANGC" --emit-c "$WORK/t_transport.m31" -o "$WORK/t_transport.c" 2>"$WORK/t_transport.diag"; then
    bad "compile t_transport" "$(cat "$WORK/t_transport.diag")"
elif ! cc -O0 -Wall -Wextra -I runtime -pthread -o "$WORK/t_transport" "$WORK/t_transport.c" \
        runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" 2>"$WORK/t_transport.cc"; then
    bad "cc t_transport" "$(cat "$WORK/t_transport.cc")"
else
    out=$("$WORK/t_transport" 2>&1)
    rc=$?
    # The stub listener closes before sending any bytes at all, which
    # `net.Conn.read_until` reports as a clean EOF (`Ok([])`, per its own
    # "empty at the end" contract), not an `io.Error` -- so this surfaces as
    # `Error.BadVersion` (no valid version line ever arrived), not
    # `Error.Io`. Either is the point: a definite, named failure, not a hang.
    if [ $rc -eq 0 ] && [[ "$out" == *"handshake ok"* ]] && [[ "$out" == *"service-accept ok"* ]] && [[ "$out" == *"wrong-host-key rejected: the server's host key does not match the expected one"* ]] && [[ "$out" == *"truncated connection rejected: the peer's SSH version line is missing or malformed"* ]]; then
        note "live handshake against a real sshd: $out"
    else
        bad "live handshake against a real sshd" "exit=$rc" "$out"
    fi
fi

echo
if [ $fail -eq 0 ]; then
    printf '\033[32mall %d apps/ssh/test_transport.sh checks passed\033[0m\n' "$pass"
else
    printf '\033[31m%d of %d apps/ssh/test_transport.sh checks FAILED\033[0m\n' "$fail" "$((pass + fail))"
fi
exit $fail
