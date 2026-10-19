#!/usr/bin/env bash
# `sshclient.connect_detailed` and `sshclient.resolve`: structured connection
# failures (unknown host, changed key, revoked key), the quiet/silent
# routing of `Warning:` lines, and the resolved destination, against a
# disposable OpenSSH `sshd` and a temporary HOME. The real `ssh-keygen` is
# the oracle for fingerprints and for the line `PendingHost.record` writes.
#
#   bash tests/ssh/test_client_failure.sh
#
# Needs `sshd` and `ssh-keygen`. Run from the repository root, with the
# compiler built (`cargo build`). sshd runs in the foreground (-D) and is
# killed by pid on exit; nothing else is started.
set -uo pipefail
cd "$(dirname "$0")/../.."
. ./runtime/arch.sh

for tool in sshd ssh-keygen; do
    command -v "$tool" >/dev/null || { echo "$tool not found on PATH -- skipping"; exit 0; }
done
SSHD_BIN=$(command -v sshd)

LANGC=./target/debug/m31c
WORK=$(mktemp -d)
server_pid=""
pass=0
fail=0
cleanup() {
    [ -n "$server_pid" ] && kill "$server_pid" >/dev/null 2>&1
    [ -n "$server_pid" ] && wait "$server_pid" 2>/dev/null
    rm -rf "$WORK"
}
trap cleanup EXIT
note() { printf '\033[32mok\033[0m   %s\n' "$1"; pass=$((pass + 1)); }
bad()  { printf '\033[31mFAIL\033[0m %s\n' "$1"; shift; printf '%s\n' "$@" | sed 's/^/     /'; fail=$((fail + 1)); }
expect() { # label, want, got
    if [ "$3" = "$2" ]; then note "$1"; else bad "$1" "want: $2" "got:  $3"; fi
}
expect_has() { # label, substring, got
    case "$3" in *"$2"*) note "$1" ;; *) bad "$1" "want substring: $2" "got: $3" ;; esac
}
expect_lacks() { # label, substring, got
    case "$3" in *"$2"*) bad "$1" "unwanted substring: $2" "got: $3" ;; *) note "$1" ;; esac
}

"$LANGC" -fmt --check tests/ssh/t_ssh_failure.m31 || { echo "FAILED: fmt"; exit 1; }
"$LANGC" -emit-c tests/ssh/t_ssh_failure.m31 --out "$WORK/t.c" 2>/dev/null || exit 1
cc -O2 -w -I runtime -pthread -o "$WORK/t" "$WORK/t.c" \
    runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" || exit 1

HOME_DIR="$WORK/home"
mkdir -p "$HOME_DIR/.ssh"
chmod 700 "$HOME_DIR/.ssh"
ssh-keygen -t ed25519 -f "$WORK/host_key" -N "" -q
ssh-keygen -t ed25519 -f "$WORK/other_key" -N "" -q
ssh-keygen -t ed25519 -f "$WORK/user_key" -N "" -q
chmod 600 "$WORK/host_key" "$WORK/user_key"
cp "$WORK/user_key.pub" "$WORK/authorized_keys"
USER_NAME=$(id -un)
PORT=$((20000 + (RANDOM % 20000)))
HOST_LINE=$(cut -d' ' -f1,2 "$WORK/host_key.pub")
OTHER_LINE=$(cut -d' ' -f1,2 "$WORK/other_key.pub")
HOST_FP=$(ssh-keygen -lf "$WORK/host_key.pub" | cut -d' ' -f2)
OTHER_FP=$(ssh-keygen -lf "$WORK/other_key.pub" | cut -d' ' -f2)
KH_NAME="[127.0.0.1]:$PORT"

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
# Many short connections from one address: newer sshd penalises them unless
# told not to. Only where this sshd knows the keyword (macOS's older one
# does not, and `sshd -t` says so).
cp "$WORK/sshd_config" "$WORK/sshd_config.try"
echo "PerSourcePenalties no" >>"$WORK/sshd_config.try"
if "$SSHD_BIN" -t -f "$WORK/sshd_config.try" >/dev/null 2>&1; then
    cp "$WORK/sshd_config.try" "$WORK/sshd_config"
fi
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

# run CONFIG_TEXT MODE HOST PORT USER [EXTRA]: stdout; stderr to $WORK/stderr.
# No agent, stdin closed: nothing could be asked of anyone.
run() {
    printf '%b' "$1" >"$HOME_DIR/.ssh/config"
    env -u SSH_AUTH_SOCK HOME="$HOME_DIR" "$WORK/t" "$2" "$3" "$4" "$5" ${6:+"$6"} 2>"$WORK/stderr" </dev/null
}

cfg() { # known_hosts file, extra lines
    printf 'Host target\n  HostName 127.0.0.1\n  Port %s\n  User %s\n  IdentityFile %s\n  UserKnownHostsFile %s\n%s' \
        "$PORT" "$USER_NAME" "$WORK/user_key" "$1" "${2:-}"
}

# --- unknown host ------------------------------------------------------------
: >"$WORK/kh_empty"
out=$(run "$(cfg "$WORK/kh_empty")" connect target 0 - silent)
expect "unknown host: structured, with host, port, key type, fingerprint and path" \
    "unknown 127.0.0.1 $PORT ssh-ed25519 $HOST_FP $WORK/kh_empty hashed=false" "$out"
expect "unknown host: the fingerprint is what ssh-keygen -lf prints" "$HOST_FP" "$(printf '%s' "$out" | cut -d' ' -f5)"
[ ! -s "$WORK/kh_empty" ] && note "unknown host: known_hosts not written" || bad "unknown host: known_hosts was written" "$(cat "$WORK/kh_empty")"
[ ! -s "$WORK/stderr" ] && note "unknown host, silent: nothing on stderr" || bad "unknown host, silent: stderr" "$(cat "$WORK/stderr")"

out=$(run "$(cfg "$WORK/kh_empty")" connect target 0 - loud)
expect_has "unknown host, not silent: same structured failure" "unknown 127.0.0.1 $PORT" "$out"
[ ! -s "$WORK/stderr" ] && note "unknown host, never asks: nothing on stderr (no prompt)" || bad "prompt or text on stderr" "$(cat "$WORK/stderr")"

out=$(run "$(cfg "$WORK/kh_empty" '  StrictHostKeyChecking yes\n')" connect target 0 - silent)
expect_has "StrictHostKeyChecking yes: still reported as unknown host" "unknown 127.0.0.1 $PORT" "$out"

# --- the approve flow: record, then connect again ------------------------------
out=$(run "$(cfg "$WORK/kh_empty")" approve target 0 -)
expect "approve: unknown, recorded, then connected" "$(printf 'unknown 127.0.0.1 %s ssh-ed25519 %s %s hashed=false\nrecorded\nconnected hi' "$PORT" "$HOST_FP" "$WORK/kh_empty")" "$out"
expect_has "approve: the file has the host and key" "$KH_NAME $HOST_LINE" "$(cat "$WORK/kh_empty")"
ssh-keygen -F "$KH_NAME" -f "$WORK/kh_empty" >/dev/null 2>&1 && note "approve: ssh-keygen -F finds the recorded line" || bad "approve: ssh-keygen -F does not find the line" "$(cat "$WORK/kh_empty")"
out=$(run "$(cfg "$WORK/kh_empty")" connect target 0 - silent)
expect "after recording, a plain connect succeeds" "connected hi" "$out"

: >"$WORK/kh_hashed"
out=$(run "$(cfg "$WORK/kh_hashed" '  HashKnownHosts yes\n')" approve target 0 -)
expect_has "HashKnownHosts: pending says hashed" "hashed=true" "$out"
expect_has "HashKnownHosts: connected after recording" "connected hi" "$out"
expect_lacks "HashKnownHosts: the file does not name the host" "127.0.0.1" "$(cat "$WORK/kh_hashed")"
ssh-keygen -F "$KH_NAME" -f "$WORK/kh_hashed" >/dev/null 2>&1 && note "HashKnownHosts: ssh-keygen -F finds the hashed line" || bad "hashed line not found" "$(cat "$WORK/kh_hashed")"

# --- changed key ---------------------------------------------------------------
printf '%s %s\n' "$KH_NAME" "$OTHER_LINE" >"$WORK/kh_changed"
before=$(cat "$WORK/kh_changed")
out=$(run "$(cfg "$WORK/kh_changed")" connect target 0 - silent)
expect "changed key: presented and listed fingerprints" \
    "changed 127.0.0.1 $PORT ssh-ed25519 $HOST_FP known=$OTHER_FP $WORK/kh_changed" "$out"
expect "changed key: known_hosts untouched" "$before" "$(cat "$WORK/kh_changed")"
out=$(run "$(cfg "$WORK/kh_changed")" approve target 0 -)
expect_lacks "changed key: approve has no record path" "recorded" "$out"
expect "changed key: still refused after the approve attempt" "$before" "$(cat "$WORK/kh_changed")"

# --- revoked key ---------------------------------------------------------------
printf '@revoked %s %s\n' "$KH_NAME" "$HOST_LINE" >"$WORK/kh_revoked"
out=$(run "$(cfg "$WORK/kh_revoked")" connect target 0 - silent)
expect_has "revoked key: HostKeyRevoked with the presented fingerprint" "revoked 127.0.0.1 $PORT ssh-ed25519 $HOST_FP" "$out"
printf '@revoked %s %s\n' "$KH_NAME" "$HOST_LINE" >"$WORK/kh_revoked_other"
printf '%s %s\n' "$KH_NAME" "$OTHER_LINE" >>"$WORK/kh_revoked_other"

# --- two failures in one program: nothing leaks from the first into the second --
out=$(run "$(cfg "$WORK/kh_changed")" twice target 0 -)
expect "two attempts report the same, one listed fingerprint each" \
    "$(printf 'changed 127.0.0.1 %s ssh-ed25519 %s known=%s %s\nchanged 127.0.0.1 %s ssh-ed25519 %s known=%s %s' "$PORT" "$HOST_FP" "$OTHER_FP" "$WORK/kh_changed" "$PORT" "$HOST_FP" "$OTHER_FP" "$WORK/kh_changed")" "$out"

# --- other failures ------------------------------------------------------------
out=$(run "$(cfg "$WORK/kh_empty")" connect 127.0.0.1 1 - silent)
expect_has "unreachable port: Other with the connect error" "other could not connect" "$out"
out=$(run "Host *\n  UserKnownHostsFile $WORK/kh_empty\n" connect 127.0.0.1 "$PORT" "$USER_NAME" silent)
expect_has "no identity: Other, not a host-key failure" "other no usable identity" "$out"

# --- Warning: lines: routed, or quiet ------------------------------------------
UNSUPPORTED='  GSSAPIAuthentication yes\n'
out=$(run "$(cfg "$WORK/kh_empty" "$UNSUPPORTED")" connect target 0 - loud)
expect_has "unsupported option, not silent: written to stderr" "Warning: configuration option" "$(cat "$WORK/stderr")"
expect_lacks "unsupported option, not silent: not returned" "warning-line" "$out"
out=$(run "$(cfg "$WORK/kh_empty" "$UNSUPPORTED")" connect target 0 - silent)
expect_has "unsupported option, silent: returned in detail.warnings" "warning-line Warning: configuration option" "$out"
[ ! -s "$WORK/stderr" ] && note "unsupported option, silent: stderr stays empty" || bad "silent but stderr written" "$(cat "$WORK/stderr")"

out=$(run "$(cfg "$WORK/kh_empty" "$UNSUPPORTED")" connect target 0 - quiet)
expect_lacks "unsupported option, is_quiet: no warning on stderr" "Warning" "$(cat "$WORK/stderr")"
expect_lacks "unsupported option, is_quiet: none returned either" "warning-line" "$out"

# a key file with loose permissions: a warning on the connect path
cp "$WORK/user_key" "$WORK/loose_key"
chmod 644 "$WORK/loose_key"
out=$(run "Host loose\n  HostName 127.0.0.1\n  Port $PORT\n  User $USER_NAME\n  IdentityFile $WORK/loose_key\n  IdentityFile $WORK/user_key\n  UserKnownHostsFile $WORK/kh_empty\n  StrictHostKeyChecking accept-new\n" connect loose 0 - silent)
expect_has "loose key permissions, silent: warning returned" "warning-line Warning: private key" "$out"
expect_has "loose key, silent: the connection still works (second identity)" "connected hi" "$out"
[ ! -s "$WORK/stderr" ] && note "loose key, silent: stderr stays empty" || bad "silent but stderr written" "$(cat "$WORK/stderr")"

# --- resolve -------------------------------------------------------------------
mkdir -p "$HOME_DIR/.ssh/conf.d"
printf 'Host included\n  HostName 10.9.8.7\n  Port 2222\n  User inc_user\n  IdentityFile %s/id_inc\n  UserKnownHostsFile %s/kh_inc\n  StrictHostKeyChecking accept-new\n  HashKnownHosts yes\n' "$WORK" "$WORK" >"$HOME_DIR/.ssh/conf.d/inc.conf"
out=$(run "Include conf.d/*.conf\nHost jumped\n  HostName 10.0.0.5\n  ProxyJump jb@bastion:2200\n" resolve included 0 -)
expect "resolve: Include expanded; host, port, user, identity, known_hosts, policy, hash" \
    "$(printf 'alias=included\nhost=10.9.8.7\nport=2222\nuser=inc_user\nidentities=%s/id_inc default=false\nknown_hosts=%s/kh_inc\npolicy=accept-new\nhashed=true\njumps=' "$WORK" "$WORK")" "$out"
out=$(run "Include conf.d/*.conf\nHost jumped\n  HostName 10.0.0.5\n  ProxyJump jb@bastion:2200\n" resolve jumped 0 someone)
expect_has "resolve: explicit user wins; defaults for the rest" "user=someone" "$out"
expect_has "resolve: default identity files when none is configured" "identities=$HOME_DIR/.ssh/id_ed25519,$HOME_DIR/.ssh/id_ecdsa,$HOME_DIR/.ssh/id_rsa default=true" "$out"
expect_has "resolve: default known_hosts, default policy" "known_hosts=$HOME_DIR/.ssh/known_hosts" "$out"
expect_has "resolve: ask is the default policy" "policy=ask" "$out"
expect_has "resolve: jump hosts as host:port" "jumps=bastion:2200" "$out"
out=$(run "Host *\n  Port 2022\n" resolve plain 7777 -)
expect_has "resolve: explicit port wins over the file" "port=7777" "$out"
[ ! -s "$WORK/stderr" ] && note "resolve: writes nothing to stderr" || bad "resolve wrote to stderr" "$(cat "$WORK/stderr")"
out=$(run "Host broken\n  Port notanumber\n" resolve broken 0 -)
expect_has "resolve: a configuration error is an Err line" "resolve failed" "$out"

echo
if [ $fail -eq 0 ]; then
    printf '\033[32mall %d tests/ssh/test_client_failure.sh checks passed\033[0m\n' "$pass"
else
    printf '\033[31m%d of %d tests/ssh/test_client_failure.sh checks FAILED\033[0m\n' "$fail" "$((pass + fail))"
fi
exit $fail
