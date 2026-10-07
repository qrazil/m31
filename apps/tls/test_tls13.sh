#!/usr/bin/env bash
# Every check for TLS 1.3's key schedule and record layer
# (`lib/tls13_schedule.m31`, `lib/tls13_record.m31`). No handshake, no
# certificates: those are later milestones. Modelled on `apps/tls/test.sh`,
# with the same rule: nothing here compares this program with itself.
#
#   bash apps/tls/test_tls13.sh
#
# The oracles are Python's `hashlib`/`hmac`, the `cryptography` package
# (OpenSSL underneath) for HKDF-Expand and ChaCha20-Poly1305, and RFC 8448's
# published values, which the schedule oracle asserts against its own result
# before printing anything. Run from anywhere; the compiler is `LANGC`
# (default `./target/debug/m31c`, built with `cargo build`).
#
# Four checks diff the program's output against an oracle's:
#   tls13_schedule    RFC 8448 section 3 and 4 secrets, keys, IVs, Finished
#   tls13_record      sealed records for a grid of sequences, types, sizes and
#                     paddings, plus every way a record can fail to open
#   tls13_connection  a scripted peer's records, then one line per refused
#                     stream (error and alert), then a loopback socket
# and one that compares with a real server:
#   live_tls13        `openssl s_server -tls1_3 -ciphersuites
#                     TLS_CHACHA20_POLY1305_SHA256`'s bytes and key log;
#                     skipped, never failed, if OpenSSL does not cooperate.
set -uo pipefail
cd "$(dirname "$0")/../.."
. ./runtime/arch.sh

LANGC=${LANGC:-./target/debug/m31c}
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
pass=0
fail=0

note() { printf '\033[32mok\033[0m   %s\n' "$1"; pass=$((pass + 1)); }
bad()  { printf '\033[31mFAIL\033[0m %s\n' "$1"; shift; printf '%s\n' "$@" | sed 's/^/     /'; fail=$((fail + 1)); }

build() {
    local name=$1
    if ! "$LANGC" --emit-c "apps/tls/$name.m31" -o "$WORK/$name.c" 2>"$WORK/$name.diag"; then
        bad "compile $name" "$(head -5 "$WORK/$name.diag")"
        return 1
    fi
    if ! cc -O2 -Wall -Wextra -I runtime -pthread -o "$WORK/$name" "$WORK/$name.c" \
           runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" \
           2>"$WORK/$name.cc"; then
        bad "cc $name" "$(head -5 "$WORK/$name.cc")"
        return 1
    fi
    return 0
}

# Run `t_<name>` with the arguments the oracle's `--args` prints (if it has
# any), run `oracle_<name>.py`, and require byte-identical output. $2 is what
# the lines are, for the pass message; $3 is "args" if the oracle makes input.
check() {
    local name=$1 what=$2 mode=${3:-}
    local -a input=()
    if ! build "t_$name"; then
        return
    fi
    if [ "$mode" = args ]; then
        if ! read -r -a input < <(python3 "apps/tls/oracle_$name.py" --args 2>"$WORK/$name.args.err"); then
            bad "$name" "oracle_$name.py --args failed" "$(tail -5 "$WORK/$name.args.err")"
            return
        fi
    fi
    timeout 120 "$WORK/t_$name" ${input[@]+"${input[@]}"} >"$WORK/$name.got" 2>"$WORK/$name.err"
    local rc=$?
    if ! python3 "apps/tls/oracle_$name.py" >"$WORK/$name.want" 2>"$WORK/$name.oracle.err"; then
        bad "$name" "oracle_$name.py failed" "$(tail -5 "$WORK/$name.oracle.err")"
    elif [ $rc -ne 0 ]; then
        bad "$name" "t_$name exited $rc" "$(tail -5 "$WORK/$name.err")"
    elif cmp -s "$WORK/$name.got" "$WORK/$name.want"; then
        note "$name: $(wc -l <"$WORK/$name.got" | tr -d ' ') lines match $what"
    else
        bad "$name" "$(diff "$WORK/$name.got" "$WORK/$name.want" | head -12 | cut -c1-200)"
    fi
}

# --- house style ---------------------------------------------------------------

if out=$(
    for f in apps/tls/tls13_*.m31 apps/tls/t_tls13_*.m31 lib/tls13_schedule.m31 lib/tls13_record.m31; do
        "$LANGC" fmt --check "$f" || echo "$f"
    done 2>&1
) && [ -z "$out" ]; then
    note "source is formatted"
else
    bad "source is not formatted (run: m31c fmt <file>)" "$out"
fi

if ! python3 -c 'import cryptography' 2>/dev/null; then
    bad "oracles" "the 'cryptography' package is not importable -- pip install cryptography"
else
    schedule_args=$(python3 apps/tls/oracle_tls13_schedule.py --args 2>"$WORK/schedule.args.err") || {
        bad "tls13_schedule" "oracle_tls13_schedule.py --args failed" "$(tail -5 "$WORK/schedule.args.err")"
        schedule_args=
    }
    if [ -n "$schedule_args" ] && build t_tls13_schedule; then
        # shellcheck disable=SC2086
        "$WORK/t_tls13_schedule" $schedule_args >"$WORK/schedule.got" 2>"$WORK/schedule.err"
        rc=$?
        if ! python3 apps/tls/oracle_tls13_schedule.py >"$WORK/schedule.want" 2>"$WORK/schedule.oracle.err"; then
            bad "tls13_schedule" "oracle failed" "$(tail -5 "$WORK/schedule.oracle.err")"
        elif [ $rc -ne 0 ]; then
            bad "tls13_schedule" "t_tls13_schedule exited $rc" "$(tail -5 "$WORK/schedule.err")"
        elif cmp -s "$WORK/schedule.got" "$WORK/schedule.want"; then
            note "tls13_schedule: $(wc -l <"$WORK/schedule.got" | tr -d ' ') lines match hashlib/hmac/OpenSSL HKDF (RFC 8448 values asserted by the oracle)"
        else
            bad "tls13_schedule" "$(diff "$WORK/schedule.got" "$WORK/schedule.want" | head -12)"
        fi
    fi
    check tls13_record "ChaCha20Poly1305 with the TLS nonce rule (grid of records, every failure to open)"
    check tls13_connection "scripted peer, refusals and loopback" args

    if ! command -v openssl >/dev/null 2>&1; then
        printf 'skip live_tls13: openssl is not installed\n'
    elif build t_tls13_live; then
        live=$(timeout 90 python3 apps/tls/live_tls13.py "$WORK/t_tls13_live" 2>&1)
        case $? in
            0) note "live_tls13: $(printf '%s\n' "$live" | tail -1)" ;;
            2) printf 'skip live_tls13: %s\n' "$(printf '%s\n' "$live" | tail -1)" ;;
            *) bad "live_tls13" "$(printf '%s\n' "$live" | tail -6)" ;;
        esac
    fi
fi

echo
if [ $fail -eq 0 ]; then
    printf '\033[32mall %d tls13 checks passed\033[0m\n' "$pass"
else
    printf '\033[31m%d of %d tls13 checks FAILED\033[0m\n' "$fail" "$((pass + fail))"
fi
exit $fail
