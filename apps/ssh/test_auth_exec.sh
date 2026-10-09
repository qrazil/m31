#!/usr/bin/env bash
# `docs/ssh-decision.md` Phases 3 and 4's live half: public-key authentication
# (`lib/sshauth.m31`, `lib/sshkey.m31`), `known_hosts` (`lib/sshhosts.m31`) and
# command execution (`lib/sshexec.m31`, through `lib/sshclient.m31`) against a
# real, independent SSH implementation -- a disposable OpenSSH `sshd` run as
# the current user in a temp directory, never touching `~/.ssh`, with freshly
# generated keys that are thrown away afterwards.
#
#   bash apps/ssh/test_auth_exec.sh
#
# Needs `sshd`, `ssh-keygen`, `sha256sum` and `seq`. The fixture is first
# checked with the real `ssh` client (when installed), so a failure of the m31
# client cannot be blamed on a broken fixture. `sshd` runs with `-D` (see
# `test_transport.sh`) and is pinned to this client's one suite.
set -uo pipefail
cd "$(dirname "$0")/../.."
. ./runtime/arch.sh

for tool in sshd ssh-keygen sha256sum seq; do
    command -v "$tool" >/dev/null || { echo "$tool not found on PATH -- skipping"; exit 0; }
done
SSHD_BIN=$(command -v sshd)

LANGC=./target/debug/m31c
WORK=$(mktemp -d)
pass=0
fail=0
server_pid=""

note() { printf '\033[32mok\033[0m   %s\n' "$1"; pass=$((pass + 1)); }
bad()  { printf '\033[31mFAIL\033[0m %s\n' "$1"; shift; printf '%s\n' "$@" | sed 's/^/     /'; fail=$((fail + 1)); }

cleanup() {
    [ -n "$server_pid" ] && kill "$server_pid" >/dev/null 2>&1
    [ -n "$server_pid" ] && wait "$server_pid" 2>/dev/null
    rm -rf "$WORK"
}
trap cleanup EXIT

# --- keys -----------------------------------------------------------------------

ssh-keygen -t ed25519 -f "$WORK/host_key" -N "" -q
ssh-keygen -t ed25519 -f "$WORK/other_host_key" -N "" -q
ssh-keygen -t ed25519 -f "$WORK/user_key" -N "" -q -C "m31 live test"
ssh-keygen -t ed25519 -f "$WORK/wrong_key" -N "" -q
ssh-keygen -t ed25519 -f "$WORK/enc_key" -N "correct horse" -q
chmod 600 "$WORK"/*_key

cat "$WORK/user_key.pub" "$WORK/enc_key.pub" >"$WORK/authorized_keys"
USER_NAME=$(id -un)
PORT=$((20000 + (RANDOM % 20000)))

# known_hosts in the three shapes the parser handles: plain `[host]:port`,
# one naming a different key, and `ssh-keygen -H` hashed.
printf '[127.0.0.1]:%s %s %s\n' "$PORT" $(cut -d' ' -f1,2 "$WORK/host_key.pub") >"$WORK/kh_plain"
printf '[127.0.0.1]:%s %s %s\n' "$PORT" $(cut -d' ' -f1,2 "$WORK/other_host_key.pub") >"$WORK/kh_wrong"
cp "$WORK/kh_plain" "$WORK/kh_hashed"
ssh-keygen -H -f "$WORK/kh_hashed" >/dev/null 2>&1
rm -f "$WORK/kh_hashed.old"
printf 'elsewhere.example %s %s\n' $(cut -d' ' -f1,2 "$WORK/host_key.pub") >"$WORK/kh_other"

cat >"$WORK/sshd_config" <<EOF
Port $PORT
ListenAddress 127.0.0.1
HostKey $WORK/host_key
PidFile $WORK/sshd.pid
AuthorizedKeysFile $WORK/authorized_keys
StrictModes no
UsePAM no
KexAlgorithms curve25519-sha256
HostKeyAlgorithms ssh-ed25519
Ciphers chacha20-poly1305@openssh.com
PubkeyAuthentication yes
PasswordAuthentication no
KbdInteractiveAuthentication no
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

# --- the fixture itself, checked with the real client -----------------------------

if command -v ssh >/dev/null; then
    real=$(ssh -F /dev/null -p "$PORT" -i "$WORK/user_key" -o IdentitiesOnly=yes \
        -o UserKnownHostsFile="$WORK/kh_plain" -o StrictHostKeyChecking=yes \
        -o BatchMode=yes "$USER_NAME@127.0.0.1" 'echo fixture-ok' 2>"$WORK/real_ssh.err")
    if [ "$real" = "fixture-ok" ]; then
        note "fixture: the real ssh client logs in and runs a command"
    else
        bad "fixture: the real ssh client cannot log in" "$(cat "$WORK/real_ssh.err")" "$(tail -5 "$WORK/sshd.log")"
        exit 1
    fi
    if ssh -F /dev/null -p "$PORT" -i "$WORK/wrong_key" -o IdentitiesOnly=yes \
        -o UserKnownHostsFile="$WORK/kh_plain" -o BatchMode=yes \
        "$USER_NAME@127.0.0.1" true >/dev/null 2>&1; then
        bad "fixture: sshd accepted a key that is not in authorized_keys"
        exit 1
    fi
else
    echo "(no ssh client installed -- fixture not cross-checked)"
fi

# --- the m31 program ---------------------------------------------------------------

SEQ_SHA=$(seq 1 1200000 | sha256sum | cut -d' ' -f1)

cat >"$WORK/t_live.m31" <<EOF
import ssh;
import sshkey;
import sshclient;
import sshexec;
import sshhosts;
import sshauth;
import sha256;

const str USER = "$USER_NAME";
const int PORT = $PORT;
const str USER_KEY = "$WORK/user_key";
const str WRONG_KEY = "$WORK/wrong_key";
const str ENC_KEY = "$WORK/enc_key";
const str KH_PLAIN = "$WORK/kh_plain";
const str KH_HASHED = "$WORK/kh_hashed";
const str KH_WRONG = "$WORK/kh_wrong";
const str KH_OTHER = "$WORK/kh_other";
const str SEQ_SHA = "$SEQ_SHA";
const int EIGHT_MB = 8388608;

int check(int n, bool ok, str label) {
    if (!ok) {
        trap("check failed: " + label);
    }
    print("ok   " + label);
    return n + 1;
}

// Which stage refused, by name.
str why(sshclient.Error e) {
    match (e) {
        case Key(sshkey.Error k): {
            match (k) {
                case Encrypted: {
                    return "key-encrypted";
                }
                case Io: {
                    return "key-io";
                }
                case NotOpenSshKey: {
                    return "key-not-openssh";
                }
                case Truncated: {
                    return "key-truncated";
                }
                case UnsupportedKeyType: {
                    return "key-type";
                }
                case UnsupportedKeyCount: {
                    return "key-count";
                }
                case BadCheck: {
                    return "key-check";
                }
                case BadKey: {
                    return "key-bad";
                }
                case BadPadding: {
                    return "key-padding";
                }
            }
        }
        case KnownHosts(sshhosts.Error h): {
            match (h) {
                case UnknownHost: {
                    return "hosts-unknown";
                }
                case Revoked: {
                    return "hosts-revoked";
                }
                case Io: {
                    return "hosts-io";
                }
                case MalformedLine: {
                    return "hosts-malformed";
                }
                case BadKey: {
                    return "hosts-badkey";
                }
            }
        }
        case Connect(ssh.Error c): {
            match (c) {
                case HostKeyMismatch: {
                    return "connect-hostkey-mismatch";
                }
                case Io: {
                    return "connect-io";
                }
                case Truncated: {
                    return "connect-truncated";
                }
                case BadSignature: {
                    return "connect-bad-signature";
                }
                case Disconnected: {
                    return "connect-disconnected";
                }
                case BadVersion: {
                    return "connect-bad-version";
                }
                case BadPacketLength: {
                    return "connect-bad-packet-length";
                }
                case MacMismatch: {
                    return "connect-mac";
                }
                case UnexpectedMessage: {
                    return "connect-unexpected";
                }
                case NoCommonKex: {
                    return "connect-kex";
                }
                case NoCommonHostKey: {
                    return "connect-hostkey-alg";
                }
                case NoCommonCipher: {
                    return "connect-cipher";
                }
                case UnsupportedHostKeyType: {
                    return "connect-hostkey-type";
                }
            }
        }
        case Auth(sshauth.Error a): {
            match (a) {
                case KeyRejected: {
                    return "auth-key-rejected";
                }
                case AuthFailed: {
                    return "auth-failed";
                }
                case Transport(ssh.Error t): {
                    return "auth-transport";
                }
                case Truncated: {
                    return "auth-truncated";
                }
                case UnexpectedMessage: {
                    return "auth-unexpected";
                }
                case BadMessage: {
                    return "auth-bad-message";
                }
            }
        }
        case Exec(sshexec.Error x): {
            return "exec: " + x.to_str();
        }
    }
}

// The connect result's refusal reason, or "connected" (closing the session).
str try_connect(str host, str user, str key, str hosts) {
    match (sshclient.connect(host, PORT, user, key, hosts)) {
        case Ok(sshclient.Session s): {
            match (s.close()) {
                case Ok(bool done): {
                }
                case Err(sshclient.Error e): {
                }
            }
            return "connected";
        }
        case Err(sshclient.Error e): {
            return why(e);
        }
    }
}

sshclient.Session open() {
    match (sshclient.connect("127.0.0.1", PORT, USER, USER_KEY, KH_PLAIN)) {
        case Ok(sshclient.Session s): {
            return s;
        }
        case Err(sshclient.Error e): {
            trap("connect failed: " + why(e) + " / " + e.to_str());
        }
    }
}

sshexec.Channel start(sshclient.Session s, str command) {
    match (s.exec(command)) {
        case Ok(sshexec.Channel ch): {
            return ch;
        }
        case Err(sshclient.Error e): {
            trap("exec \"" + command + "\" failed: " + e.to_str());
        }
    }
}

bytes slurp(sshexec.Channel ch) {
    match (ch.read_all()) {
        case Ok(bytes b): {
            return b;
        }
        case Err(sshexec.Error e): {
            trap("read_all: " + e.to_str());
        }
    }
}

bytes slurp_err(sshexec.Channel ch) {
    match (ch.read_all_err()) {
        case Ok(bytes b): {
            return b;
        }
        case Err(sshexec.Error e): {
            trap("read_all_err: " + e.to_str());
        }
    }
}

// The exit status, -1 if unknown, -2 if the command died of a signal.
int finish(sshexec.Channel ch) {
    match (ch.wait()) {
        case Ok(sshexec.Exit x): {
            match (x) {
                case Status(int code): {
                    return code;
                }
                case Signal(str name): {
                    return -2;
                }
                case Unknown: {
                    return -1;
                }
            }
        }
        case Err(sshexec.Error e): {
            trap("wait: " + e.to_str());
        }
    }
}

void put(sshexec.Channel ch, bytes data) {
    match (ch.write(data)) {
        case Ok(int n): {
            if (n != data.size()) {
                trap("short write");
            }
        }
        case Err(sshexec.Error e): {
            trap("write: " + e.to_str());
        }
    }
}

void eof(sshexec.Channel ch) {
    match (ch.close_write()) {
        case Ok: {
        }
        case Err(sshexec.Error e): {
            trap("close_write: " + e.to_str());
        }
    }
}

// A position-dependent pattern, so lost, repeated or reordered bytes change
// the digest.
bytes pattern(int total) {
    bytes block = [];
    int i = 0;
    while (i < 65536) {
        block.push((i * 7 + i / 251 + 13) & 255);
        i = i + 1;
    }
    bytes out = [];
    while (out.size() < total) {
        out.extend(block);
    }
    return out.substr(0, total);
}

int checks = 0;

// --- who may log in ---
checks = check(checks, try_connect("127.0.0.1", USER, USER_KEY, KH_PLAIN) == "connected", "the right key is accepted");
checks = check(checks, try_connect("127.0.0.1", USER, WRONG_KEY, KH_PLAIN) == "auth-key-rejected", "a key that is not authorized is refused");
checks = check(checks, try_connect("127.0.0.1", "nosuchuser_m31", USER_KEY, KH_PLAIN) == "auth-key-rejected", "an unknown user is refused");
checks = check(checks, try_connect("127.0.0.1", USER, ENC_KEY, KH_PLAIN) == "key-encrypted", "a passphrase-protected key is refused before any connection");
checks = check(checks, try_connect("127.0.0.1", USER, USER_KEY + ".missing", KH_PLAIN) == "key-io", "a missing key file is refused");

// --- which servers are trusted ---
checks = check(checks, try_connect("127.0.0.1", USER, USER_KEY, KH_HASHED) == "connected", "a hashed known_hosts (ssh-keygen -H) is honoured");
checks = check(checks, try_connect("127.0.0.1", USER, USER_KEY, KH_WRONG) == "connect-hostkey-mismatch", "a known_hosts naming another key refuses the host");
checks = check(checks, try_connect("127.0.0.1", USER, USER_KEY, KH_OTHER) == "hosts-unknown", "a host that is not listed is refused (no trust on first use)");
checks = check(checks, try_connect("localhost", USER, USER_KEY, KH_PLAIN) == "hosts-unknown", "a different name for the same host is not listed");
checks = check(checks, try_connect("127.0.0.1", USER, USER_KEY, KH_PLAIN + ".missing") == "hosts-io", "a missing known_hosts is refused");

// --- commands, all on one connection ---
sshclient.Session s = open();

sshexec.Channel c1 = start(s, "echo hello");
bytes o1 = slurp(c1);
checks = check(checks, o1 == "hello\n".to_bytes(), "exec echo: stdout is hello");
checks = check(checks, finish(c1) == 0, "exec echo: exit status 0");

sshexec.Channel c2 = start(s, "exit 3");
checks = check(checks, slurp(c2).size() == 0, "exit 3: no output");
checks = check(checks, finish(c2) == 3, "exit 3: exit status 3 (second command on the same connection)");

sshexec.Channel c3 = start(s, "echo out; echo err >&2; exit 5");
bytes o3 = slurp(c3);
bytes e3 = slurp_err(c3);
checks = check(checks, o3 == "out\n".to_bytes(), "stderr separation: stdout holds only stdout");
checks = check(checks, e3 == "err\n".to_bytes(), "stderr separation: stderr holds only stderr");
checks = check(checks, finish(c3) == 5, "stderr separation: exit status 5");

sshexec.Channel c4 = start(s, "no-such-command-m31 2>/dev/null");
checks = check(checks, slurp(c4).size() == 0, "an unknown command prints nothing on stdout");
checks = check(checks, finish(c4) == 127, "an unknown command exits 127");

sshexec.Channel c5 = start(s, "cat");
eof(c5);
checks = check(checks, slurp(c5).size() == 0, "cat with immediate EOF on stdin produces nothing");
checks = check(checks, finish(c5) == 0, "cat with immediate EOF exits 0");

sshexec.Channel c6 = start(s, "kill -9 \$\$");
bytes o6 = slurp(c6);
checks = check(checks, finish(c6) == -2, "a command killed by a signal reports a signal exit");

// --- large transfers ---
sshexec.Channel r1 = start(s, "head -c 8388608 /dev/zero");
bytes zeros = slurp(r1);
bool all_zero = true;
int zi = 0;
while (zi < zeros.size()) {
    if (zeros[zi] != 0) {
        all_zero = false;
    }
    zi = zi + 1024;
}
checks = check(checks, zeros.size() == EIGHT_MB && all_zero, "read 8 MB from head -c: exact length");
checks = check(checks, finish(r1) == 0, "read 8 MB: exit status 0");

sshexec.Channel r2 = start(s, "seq 1 1200000");
bytes seq_out = slurp(r2);
checks = check(checks, sha256.hex(seq_out) == SEQ_SHA, "read seq 1 1200000: digest matches sha256sum");
checks = check(checks, finish(r2) == 0, "read seq: exit status 0");

bytes big = pattern(EIGHT_MB);

sshexec.Channel w1 = start(s, "wc -c");
put(w1, big);
eof(w1);
bytes wc_out = slurp(w1);
checks = check(checks, wc_out == "8388608\n".to_bytes(), "write 8 MB to wc -c: counted exactly");
checks = check(checks, finish(w1) == 0, "write 8 MB: exit status 0");

sshexec.Channel w2 = start(s, "sha256sum");
put(w2, big);
eof(w2);
bytes sum_out = slurp(w2);
checks = check(checks, sum_out == (sha256.hex(big) + "  -\n").to_bytes(), "write 8 MB to sha256sum: digest matches");
checks = check(checks, finish(w2) == 0, "write 8 MB to sha256sum: exit status 0");

bytes six = pattern(6291456);
sshexec.Channel w3 = start(s, "cat");
put(w3, six);
eof(w3);
bytes echoed = slurp(w3);
checks = check(checks, echoed.size() == six.size() && sha256.hex(echoed) == sha256.hex(six), "cat round trip of 6 MB (write and read interleaved)");
checks = check(checks, finish(w3) == 0, "cat round trip: exit status 0");

// --- the unread stream must not wedge the connection ---
sshexec.Channel f1 = start(s, "head -c 3000000 /dev/zero >&2; echo done");
bytes f1o = slurp(f1);
checks = check(checks, f1o == "done\n".to_bytes(), "a 3 MB stderr flood while reading only stdout");
bytes f1e = slurp_err(f1);
checks = check(checks, f1e.size() == 3000000, "the flooded stderr is all there afterwards");
checks = check(checks, finish(f1) == 0, "stderr flood: exit status 0");

// --- giving up early, then carrying on ---
sshexec.Channel y = start(s, "yes");
match (y.read(100)) {
    case Ok(bytes first): {
        checks = check(checks, first.size() > 0 && first[0] == 'y', "yes: reads the start of an endless stream");
    }
    case Err(sshexec.Error e): {
        trap("read yes: " + e.to_str());
    }
}
match (y.close()) {
    case Ok: {
    }
    case Err(sshexec.Error e): {
        trap("close yes: " + e.to_str());
    }
}
checks = check(checks, true, "yes: close() returns once the server confirms");

sshexec.Channel z = start(s, "echo after");
checks = check(checks, slurp(z) == "after\n".to_bytes(), "a command after an abandoned one still works");
checks = check(checks, finish(z) == 0, "after: exit status 0");

match (s.close()) {
    case Ok(bool done): {
    }
    case Err(sshclient.Error e): {
        trap("close: " + e.to_str());
    }
}
print(checks.to_str() + " checks, 0 failures");
EOF

if ! "$LANGC" --emit-c "$WORK/t_live.m31" -o "$WORK/t_live.c" 2>"$WORK/t_live.diag"; then
    bad "compile t_live" "$(head -20 "$WORK/t_live.diag")"
elif ! cc -O2 -Wall -Wextra -I runtime -pthread -o "$WORK/t_live" "$WORK/t_live.c" \
        runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" 2>"$WORK/t_live.cc"; then
    bad "cc t_live" "$(head -20 "$WORK/t_live.cc")"
else
    timeout 300 "$WORK/t_live" >"$WORK/t_live.out" 2>"$WORK/t_live.err"
    rc=$?
    if [ $rc -eq 0 ] && tail -1 "$WORK/t_live.out" | grep -q ' checks, 0 failures$'; then
        note "live client against a real sshd: $(tail -1 "$WORK/t_live.out")"
    else
        bad "live client against a real sshd" "exit=$rc" "$(tail -8 "$WORK/t_live.out")" "$(tail -5 "$WORK/t_live.err")" "sshd: $(tail -5 "$WORK/sshd.log")"
    fi
fi

echo
if [ $fail -eq 0 ]; then
    printf '\033[32mall %d apps/ssh/test_auth_exec.sh checks passed\033[0m\n' "$pass"
else
    printf '\033[31m%d of %d apps/ssh/test_auth_exec.sh checks FAILED\033[0m\n' "$fail" "$((pass + fail))"
fi
exit $fail
