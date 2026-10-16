#!/usr/bin/env bash
# `sshclient.connect_configured`: the library entry point that connects the
# way `ssh user@host` would (config file, agent, default identities, host-key
# policy), against a disposable OpenSSH `sshd` and a private `ssh-agent`, with
# a temporary HOME. The real `ssh` is the oracle for what the fixture accepts.
#
#   bash tests/ssh/test_client_api.sh
#
# Needs `sshd`, `ssh-keygen`, `ssh-agent` and `ssh-add`. Run from the
# repository root, with the compiler built (`cargo build`).
set -uo pipefail
cd "$(dirname "$0")/../.."
. ./runtime/arch.sh

for tool in sshd ssh-keygen ssh-agent ssh-add; do
    command -v "$tool" >/dev/null || { echo "$tool not found on PATH -- skipping"; exit 0; }
done
SSHD_BIN=$(command -v sshd)

LANGC=./target/debug/m31c
WORK=$(mktemp -d)
server_pid=""
agent_pid=""
pass=0
fail=0
cleanup() {
    [ -n "$server_pid" ] && kill "$server_pid" >/dev/null 2>&1
    [ -n "$agent_pid" ] && kill "$agent_pid" >/dev/null 2>&1
    wait 2>/dev/null
    rm -rf "$WORK"
}
trap cleanup EXIT
note() { printf '\033[32mok\033[0m   %s\n' "$1"; pass=$((pass + 1)); }
bad()  { printf '\033[31mFAIL\033[0m %s\n' "$1"; shift; printf '%s\n' "$@" | sed 's/^/     /'; fail=$((fail + 1)); }

"$LANGC" fmt --check tests/ssh/t_ssh_client_api.m31 || { echo "FAILED: fmt"; exit 1; }
"$LANGC" --emit-c tests/ssh/t_ssh_client_api.m31 -o "$WORK/t.c" || exit 1
cc -O2 -w -I runtime -pthread -o "$WORK/t" "$WORK/t.c" \
    runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" || exit 1

HOME_DIR="$WORK/home"
mkdir -p "$HOME_DIR/.ssh"
chmod 700 "$HOME_DIR/.ssh"
ssh-keygen -t ed25519 -f "$WORK/host_key" -N "" -q
ssh-keygen -t ed25519 -f "$WORK/user_key" -N "" -q
ssh-keygen -t ed25519 -f "$WORK/locked_key" -N "secret-pass" -q
chmod 600 "$WORK/host_key" "$WORK/user_key" "$WORK/locked_key"
cp "$WORK/user_key.pub" "$WORK/authorized_keys"
cat "$WORK/locked_key.pub" >>"$WORK/authorized_keys"
USER_NAME=$(id -un)
PORT=$((20000 + (RANDOM % 20000)))
HOSTKEY_LINE=$(cut -d' ' -f1,2 "$WORK/host_key.pub")
printf '[127.0.0.1]:%s %s\n' "$PORT" "$HOSTKEY_LINE" >"$WORK/kh_good"
: >"$WORK/kh_empty"

cat >"$WORK/sshd_config" <<EOT
Port $PORT
ListenAddress 127.0.0.1
HostKey $WORK/host_key
PidFile $WORK/sshd.pid
AuthorizedKeysFile $WORK/authorized_keys
StrictModes no
UsePAM no
PubkeyAuthentication yes
PasswordAuthentication no
KbdInteractiveAuthentication no
EOT
"$SSHD_BIN" -f "$WORK/sshd_config" -D -e >"$WORK/sshd.log" 2>&1 &
server_pid=$!
up=0
for _ in $(seq 1 100); do
    if (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then
        exec 3>&-
        up=1
        break
    fi
    sleep 0.05
done
[ "$up" = 1 ] || { bad "nothing listening on $PORT" "$(cat "$WORK/sshd.log")"; exit 1; }

eval "$(ssh-agent -s -a "$WORK/agent.sock")" >/dev/null
agent_pid=$SSH_AGENT_PID
unset SSH_AGENT_PID
unset SSH_AUTH_SOCK

# run CONFIG_TEXT USE_AGENT HOST PORT USER COMMAND
run() {
    printf '%b' "$1" >"$HOME_DIR/.ssh/config"
    local sock=""
    [ "$2" = agent ] && sock="$WORK/agent.sock"
    env -u SSH_AUTH_SOCK HOME="$HOME_DIR" ${sock:+SSH_AUTH_SOCK="$sock"} \
        "$WORK/t" "$3" "$4" "$5" "$6" 2>/dev/null </dev/null
}
expect() { # label, want, got
    if [ "$3" = "$2" ]; then note "$1"; else bad "$1" "want: $2" "got:  $3"; fi
}
expect_has() { # label, substring, got
    case "$3" in *"$2"*) note "$1" ;; *) bad "$1" "want substring: $2" "got: $3" ;; esac
}

CFG_ALIAS="Host alias\n  HostName 127.0.0.1\n  Port $PORT\n  User $USER_NAME\n  IdentityFile $WORK/user_key\n  UserKnownHostsFile $WORK/kh_good\n"
expect "config alias: HostName/Port/User/IdentityFile/UserKnownHostsFile, remote exit status" \
    "$(printf 'hi\nexit status 3')" "$(run "$CFG_ALIAS" none alias 0 - 'echo hi; exit 3')"

# oracle: real ssh accepts the same configuration
if command -v ssh >/dev/null; then
    printf '%b' "$CFG_ALIAS" >"$WORK/oracle_config"
    got=$(ssh -F "$WORK/oracle_config" -o BatchMode=yes alias 'echo hi' 2>/dev/null)
    expect "oracle: real ssh with the same config reaches the same server" "hi" "$got"
fi

SSH_AUTH_SOCK="$WORK/agent.sock" ssh-add -q "$WORK/user_key"
CFG_AGENT="Host viaagent\n  HostName 127.0.0.1\n  Port $PORT\n  User $USER_NAME\n  UserKnownHostsFile $WORK/kh_good\n"
expect "agent only: no key file, SSH_AUTH_SOCK signs" "$(printf 'hi\nexit status 0')" \
    "$(run "$CFG_AGENT" agent viaagent 0 - 'echo hi')"
expect_has "no agent, no key file: no usable identity" "no usable identity" \
    "$(run "$CFG_AGENT" none viaagent 0 - 'echo hi')"
expect "explicit host, port and user override the file (nothing configured)" "$(printf 'hi\nexit status 0')" \
    "$(run "Host *\n  UserKnownHostsFile $WORK/kh_good\n" agent 127.0.0.1 "$PORT" "$USER_NAME" 'echo hi')"

CFG_UNKNOWN="Host unk\n  HostName 127.0.0.1\n  Port $PORT\n  User $USER_NAME\n  IdentityFile $WORK/user_key\n  UserKnownHostsFile $WORK/kh_empty\n"
expect_has "unknown host, not interactive: refused" "failed: ssh:" "$(run "$CFG_UNKNOWN" none unk 0 - 'echo hi')"
[ ! -s "$WORK/kh_empty" ] && note "unknown host: known_hosts not written" || bad "unknown host: known_hosts was written" "$(cat "$WORK/kh_empty")"
expect "accept-new records the host and connects" "$(printf 'hi\nexit status 0')" \
    "$(run "${CFG_UNKNOWN}  StrictHostKeyChecking accept-new\n" none unk 0 - 'echo hi')"
expect_has "accept-new wrote the key" "$HOSTKEY_LINE" "$(cat "$WORK/kh_empty")"

CFG_LOCKED="Host locked\n  HostName 127.0.0.1\n  Port $PORT\n  User $USER_NAME\n  IdentityFile $WORK/locked_key\n  IdentitiesOnly yes\n  UserKnownHostsFile $WORK/kh_good\n"
expect_has "passphrase-protected key, not interactive, no agent: skipped, denied" "Permission denied" \
    "$(run "$CFG_LOCKED" none locked 0 - 'echo hi')"
expect_has "unreachable port: one-line error" "could not connect" "$(run "$CFG_ALIAS" none 127.0.0.1 1 - 'true')"

# --- rekeying: the server decides (RekeyLimit), the client must follow ----------
# A server that rekeys every 100 KB while 2 MB of output flows: the data must
# arrive intact and the log must show keys really were exchanged again.
rekey_case() { # label, Ciphers, MACs, [client RekeyLimit: the server then keeps its default]
    local rport=$((PORT + 1)) rpid="" up=0
    sed -e "s/^Port .*/Port $rport/" -e "s/^PidFile .*/PidFile ${WORK//\//\\/}\/sshd_r.pid/" "$WORK/sshd_config" >"$WORK/sshd_rekey"
    local server_limit="RekeyLimit 100K"
    [ -n "${4:-}" ] && server_limit="RekeyLimit default"
    printf '%s\nLogLevel DEBUG2\nCiphers %s\nMACs %s\n' "$server_limit" "$2" "$3" >>"$WORK/sshd_rekey"
    "$SSHD_BIN" -f "$WORK/sshd_rekey" -D -e >"$WORK/sshd_rekey.log" 2>&1 &
    rpid=$!
    for _ in $(seq 1 100); do
        if (exec 3<>"/dev/tcp/127.0.0.1/$rport") 2>/dev/null; then exec 3>&-; up=1; break; fi
        sleep 0.05
    done
    if [ "$up" != 1 ]; then
        bad "$1: sshd did not start" "$(tail -3 "$WORK/sshd_rekey.log")"
        kill "$rpid" 2>/dev/null; wait "$rpid" 2>/dev/null
        return
    fi
    printf '[127.0.0.1]:%s %s\n' "$rport" "$HOSTKEY_LINE" >"$WORK/kh_rekey"
    local cfg="Host rk\n  HostName 127.0.0.1\n  Port $rport\n  User $USER_NAME\n  IdentityFile $WORK/user_key\n  UserKnownHostsFile $WORK/kh_rekey\n"
    [ -n "${4:-}" ] && cfg="$cfg  RekeyLimit $4\n"
    local want got
    want="$(seq 1 300000)
exit status 0"
    got=$(run "$cfg" none rk 0 - 'seq 1 300000')
    kill "$rpid" 2>/dev/null; wait "$rpid" 2>/dev/null
    local count
    count=$(grep -c "rekey out after\|kex_input_kexinit\|SSH2_MSG_KEXINIT sent" "$WORK/sshd_rekey.log")
    if [ "$got" = "$want" ]; then note "$1: 2 MB survives rekeying"; else bad "$1: data lost or session ended" "got $(printf '%s' "$got" | wc -c) bytes: $(printf '%s' "$got" | tail -c 80)" "$(grep -i 'rekey\|error\|kex' "$WORK/sshd_rekey.log" | tail -4)"; fi
    if [ "$count" -ge 5 ]; then note "$1: server rekeyed $count times (log)"; else bad "$1: server did not rekey enough" "kex log lines: $count"; fi
}
rekey_case "chacha20-poly1305" "chacha20-poly1305@openssh.com" "hmac-sha2-256-etm@openssh.com"
rekey_case "aes256-gcm" "aes256-gcm@openssh.com" "hmac-sha2-256-etm@openssh.com"
rekey_case "aes128-ctr + hmac-sha2-256-etm" "aes128-ctr" "hmac-sha2-256-etm@openssh.com"
# the client's own limit (RekeyLimit in its config), the server's left at default
rekey_case "client-initiated, chacha20-poly1305" "chacha20-poly1305@openssh.com" "hmac-sha2-256-etm@openssh.com" 100K
rekey_case "client-initiated, aes256-gcm" "aes256-gcm@openssh.com" "hmac-sha2-256-etm@openssh.com" 100K

echo
if [ $fail -eq 0 ]; then
    printf '\033[32mall %d tests/ssh/test_client_api.sh checks passed\033[0m\n' "$pass"
else
    printf '\033[31m%d of %d tests/ssh/test_client_api.sh checks FAILED\033[0m\n' "$fail" "$((pass + fail))"
fi
exit $fail
