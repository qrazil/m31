#!/usr/bin/env bash
# The command lines of the four SSH programs (apps/): the grammar is `lib/args`,
# Oro's (docs/cli-conventions.md): `--port 22`, `--port=22`, `--p 22`, `-word` is
# a command. Every option parses, the errors and their exit status, the
# generated --help, --version and --completions, and the OpenSSH spellings that
# still work for one release (and warn). No server is needed: connections are
# made to a port nothing listens on.
#
#   bash tests/ssh/test_cli.sh
#
# bash 3.2 and BSD tools safe: no mapfile, no GNU sed -i, no `timeout`.
set -uo pipefail
cd "$(dirname "$0")/../.."
. ./runtime/arch.sh

LANGC=./target/debug/m31c
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
pass=0
fail=0
note() { printf '\033[32mok\033[0m   %s\n' "$1"; pass=$((pass + 1)); }
bad() { printf '\033[31mFAIL\033[0m %s\n' "$1"; shift; printf '%s\n' "$@" | sed 's/^/     /'; fail=$((fail + 1)); }

for app in m31ssh m31scp m31sftp m31ssh-keygen; do
    if ! "$LANGC" -emit-c "apps/$app/$app.m31" --out "$WORK/$app.c" 2>"$WORK/$app.diag"; then
        bad "compile $app" "$(head -5 "$WORK/$app.diag")"
        exit 1
    fi
    cc -O2 -w -I runtime -pthread -o "$WORK/$app" "$WORK/$app.c" \
        runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" || exit 1
done

# run APP args...  -> sets $out (stdout), $err (stderr), $code
run() {
    local app=$1
    shift
    "$WORK/$app" "$@" >"$WORK/out" 2>"$WORK/err" </dev/null
    code=$?
    out=$(cat "$WORK/out")
    err=$(cat "$WORK/err")
}
# expect LABEL WANT_CODE [stdout-substring] [stderr-substring]   (after a run)
expect() {
    local label=$1 want=$2 so=${3:-} se=${4:-}
    if [ "$code" != "$want" ]; then
        bad "$label" "exit $code, want $want" "stdout: $out" "stderr: $err"
    elif [ -n "$so" ] && [[ "$out" != *"$so"* ]]; then
        bad "$label" "stdout lacks: $so" "stdout: $out"
    elif [ -n "$se" ] && [[ "$err" != *"$se"* ]]; then
        bad "$label" "stderr lacks: $se" "stderr: $err"
    else
        note "$label"
    fi
}

# --- m31ssh (usage errors exit 255) ------------------------------------------------
run m31ssh --help;      expect "m31ssh --help" 0 "usage: m31ssh [options] <[USER@]HOST> [<COMMAND>...]"
for want in "--p, --port PORT" "--i, --identity FILE..." "--L, --local-forward SPEC..." "--tty-force" "--forward-only" "--version" "--completions SHELL"; do
    run m31ssh --help
    if [[ "$out" == *"$want"* ]]; then note "m31ssh --help lists: $want"; else bad "m31ssh --help lacks: $want" "$out"; fi
done
run m31ssh --version;   expect "m31ssh --version" 0 "m31ssh 0.1.0 (UNAUDITED)"
run m31ssh -h;          expect "m31ssh -h (OpenSSH style) still prints the help, with a warning" 0 "usage: m31ssh" "deprecated"
run m31ssh --completions bash; expect "m31ssh --completions bash" 0 "complete -o filenames -F _m31ssh_complete m31ssh"
if printf '%s\n' "$out" | bash -n 2>"$WORK/syntax"; then note "m31ssh --completions bash is valid bash"; else bad "bash completion syntax" "$(cat "$WORK/syntax")"; fi
run m31ssh --completions fish; expect "m31ssh --completions fish" 0 "complete -c m31ssh -l port"
run m31ssh --completions csh;  expect "m31ssh --completions csh" 255 "" "needs bash, zsh or fish"
run m31ssh;             expect "m31ssh: no host" 255 "" "missing <[USER@]HOST>"
run m31ssh --port abc h; expect "m31ssh: bad port" 255 "" "'--port' needs a whole number, and got 'abc'"
run m31ssh --port=0 h;  expect "m31ssh: port 0" 255 "" "'--port' needs a whole number from 1 to 65535, and got '0'"
run m31ssh h --port;    expect "m31ssh: --port at the end" 255 "" "'--port' needs a value and is the last argument"
run m31ssh --prot 22 h; expect "m31ssh: typo suggests" 255 "" "did you mean '--port'?"
run m31ssh --p 1 --p 2 h; expect "m31ssh: --p twice" 255 "" "given more than once"
run m31ssh --tt h;      expect "m31ssh: shorts do not cluster" 255 "" "short options do not cluster"
run m31ssh --zz h;      expect "m31ssh: unknown option" 255 "" "unknown option '--zz'"
run m31ssh --escape ab h; expect "m31ssh: --escape two chars" 255 "" "'--escape' needs one character or none"
run m31ssh --quiet=1 h; expect "m31ssh: a flag takes no value" 255 "" "is a flag and takes no value"
run m31ssh host extra --;  expect "m31ssh: a bare -- ends the old style (warns)" 255 "" "deprecated"
# every spelling reaches the connection code: the error names the host we gave
for spelling in "--port 1" "--port=1" "--p 1"; do
    # shellcheck disable=SC2086
    run m31ssh 127.0.0.1 $spelling --quiet --option StrictHostKeyChecking=no
    if [ "$code" = 255 ] && [[ "$err" != *"unknown option"* && "$err" != *"needs a"* ]]; then
        note "m31ssh $spelling parses and connects to port 1"
    else
        bad "m31ssh $spelling" "exit $code" "$err"
    fi
done
run m31ssh 127.0.0.1 "ls -l" --user nobody --config /nonexistent/config --jump 127.0.0.1:1 --escape none --tty --tty-force --quiet --stdin-null --forward-only --identity /nonexistent/k1 /nonexistent/k2 --option Port=1
if [ "$code" = 255 ] && [[ "$err" != *"unknown option"* && "$err" != *"needs a"* && "$err" != *"missing"* ]]; then
    note "m31ssh: every option parses (the remote command has a dash in it, as one operand)"
else
    bad "m31ssh: options" "exit $code" "$err"
fi
# the OpenSSH spelling: warns, and is read by the old parser
run m31ssh -p 1 -q -o StrictHostKeyChecking=no 127.0.0.1 true
expect "m31ssh -p 1 (OpenSSH style) still works, with a warning" 255 "" "OpenSSH-style options"
run m31ssh -V;          expect "m31ssh -V (OpenSSH style)" 0 "m31ssh 0.1.0 (UNAUDITED)" "deprecated"

# --- m31scp (usage errors exit 1) ---------------------------------------------------
run m31scp --help;      expect "m31scp --help" 0 "usage: m31scp [options] [<PATH>...]"
run m31scp --version;   expect "m31scp --version" 0 "m31scp 0.1.0 (UNAUDITED)"
run m31scp --completions zsh; expect "m31scp --completions zsh" 0 "#compdef m31scp"
run m31scp;             expect "m31scp: no operands" 1 "" "m31scp"
run m31scp onlyone;     expect "m31scp: one operand" 1
run m31scp a 127.0.0.1:c --port x; expect "m31scp: bad port" 1 "" "'--port' needs a whole number, and got 'x'"
run m31scp a 127.0.0.1:c --recusive; expect "m31scp: typo suggests" 1 "" "did you mean '--recursive'?"
run m31scp a 127.0.0.1:c --port 1 --recursive --preserve --quiet --config /none --jump 127.0.0.1:1 --identity /none --option Port=1
note "m31scp: every option parses ($code)"
run m31scp a 127.0.0.1:c --p 1 --r --q --c /none
note "m31scp: the short forms parse ($code)"
run m31scp -P 1 -r a 127.0.0.1:c
expect "m31scp -P 1 -r (OpenSSH style) still works, with a warning" 255 "" "OpenSSH-style options"

# --- m31sftp ---------------------------------------------------------------------------
run m31sftp --help;     expect "m31sftp --help" 0 "--b, --batch-file FILE"
run m31sftp --version;  expect "m31sftp --version" 0 "m31sftp 0.1.0 (UNAUDITED)"
run m31sftp;            expect "m31sftp: no destination" 1 "" "missing <HOST>"
run m31sftp a b;        expect "m31sftp: extra operand" 1 "" "unexpected argument 'b'"
run m31sftp h --port 99999; expect "m31sftp: bad port" 1 "" "'--port' needs a whole number from 1 to 65535, and got '99999'"
run m31sftp h --batch;  expect "m31sftp: typo suggests" 1 "" "did you mean '--batch-file'?"
run m31sftp -b x 127.0.0.1;  expect "m31sftp -b x (OpenSSH style) still works, with a warning" 255 "" "OpenSSH-style options"

# --- m31ssh-keygen (every error exits 1) -----------------------------------------------
run m31ssh-keygen --help; expect "m31ssh-keygen --help" 0 "-generate"
run m31ssh-keygen --version; expect "m31ssh-keygen --version" 0 "m31ssh-keygen 0.1.0 (UNAUDITED)"
run m31ssh-keygen -generate --rounds 0; expect "m31ssh-keygen: rounds range" 1 "" "'--rounds' needs a whole number from 1 to 1024, and got '0'"
run m31ssh-keygen -generate --fle x; expect "m31ssh-keygen: typo suggests" 1 "" "did you mean '--file'?"
run m31ssh-keygen -genrate; expect "m31ssh-keygen: a mistyped command is suggested" 1 "" "did you mean '-generate'?"
run m31ssh-keygen generate; expect "m31ssh-keygen: a command needs its dash" 1 "" "is a command, so it is written with a dash: '-generate'"
run m31ssh-keygen -remove; expect "m31ssh-keygen: -remove needs a host" 1 "" "missing <HOST>"
run m31ssh-keygen -generate --quiet --file "$WORK/k1" --new-passphrase= --comment one
expect "m31ssh-keygen -generate with every option" 0
run m31ssh-keygen -g; expect "m31ssh-keygen: -g is read as an OpenSSH option (warns)" 1 "" "deprecated"
run m31ssh-keygen -generate --q --f "$WORK/k2" --new-passphrase= --comment two; expect "m31ssh-keygen: the short forms" 0
[ -f "$WORK/k1.pub" ] && [ -f "$WORK/k2.pub" ] && note "m31ssh-keygen: both key pairs written" || bad "m31ssh-keygen: key files missing"
run m31ssh-keygen -public --file "$WORK/k1"; expect "m31ssh-keygen -public" 0 "ssh-ed25519 "
first=$out
run m31ssh-keygen -fingerprint --file "$WORK/k1"; expect "m31ssh-keygen -fingerprint" 0 "SHA256:"
fp=$out
run m31ssh-keygen -fingerprint --file "$WORK/k1" -public --file "$WORK/k1"
[[ "$out" == *"$fp"* && "$out" == *"$first"* ]] && note "m31ssh-keygen: two commands on one line run in order" || bad "two commands" "$out"
run m31ssh-keygen -generate --quiet --file "$WORK/k1";   expect "m31ssh-keygen: never overwrites" 1
printf 'example.org ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n' > "$WORK/known_hosts"
run m31ssh-keygen -remove example.org --file "$WORK/known_hosts"; expect "m31ssh-keygen -remove" 0
[ -f "$WORK/known_hosts.old" ] && note "m31ssh-keygen -remove keeps the .old file" || bad "m31ssh-keygen -remove: no .old"
# the OpenSSH spelling
run m31ssh-keygen -y -f "$WORK/k1"; expect "m31ssh-keygen -y -f (OpenSSH style)" 0 "$first" "deprecated"
run m31ssh-keygen -l -f "$WORK/k1"; expect "m31ssh-keygen -l -f (OpenSSH style)" 0 "$fp" "deprecated"
run m31ssh-keygen -q -f "$WORK/k3" -N '' -C three; expect "m31ssh-keygen -q -f -N -C (OpenSSH style)" 0 "" "deprecated"
[ -f "$WORK/k3.pub" ] && note "m31ssh-keygen: the OpenSSH-style run wrote the key" || bad "m31ssh-keygen: no k3"

echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
