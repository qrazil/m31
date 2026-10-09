#!/usr/bin/env bash
# Every check for `docs/ssh-decision.md` Phase 1 (the crypto primitives in
# `lib/sha256.m31` and `lib/chacha20poly1305.m31`) and Phase 2's pure,
# no-`sshd` half (`lib/ssh.m31`'s wire-format functions) -- no networking,
# nothing wired into `apps/git`. Modelled directly on `apps/git/test.sh`,
# with the same rule: nothing here compares this program with itself.
# `test_transport.sh`, in this same directory, is Phase 2's other half --
# the live, disposable-`sshd` fixture that this script's pure checks cannot
# stand in for.
#
#   bash apps/ssh/test.sh
#
# The oracles are Python's `hashlib` for SHA-256, the `cryptography` package
# (OpenSSL underneath) for ChaCha20, Poly1305, the AEAD construction and
# `openssh_block`'s own word layout, and plain `hashlib` arithmetic again for
# `lib/ssh.m31`'s exchange hash and key derivation -- real, independent
# implementations, not this project's own code checked against itself.
# `pip install cryptography` first.
#
# Run from the repository root, with the compiler built (`cargo build`).
set -uo pipefail
cd "$(dirname "$0")/../.."
. ./runtime/arch.sh

LANGC=./target/debug/m31c
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
pass=0
fail=0

note() { printf '\033[32mok\033[0m   %s\n' "$1"; pass=$((pass + 1)); }
bad()  { printf '\033[31mFAIL\033[0m %s\n' "$1"; shift; printf '%s\n' "$@" | sed 's/^/     /'; fail=$((fail + 1)); }

build() {
    local name=$1
    if ! "$LANGC" --emit-c "apps/ssh/$name.m31" -o "$WORK/$name.c" 2>"$WORK/$name.diag"; then
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

# --- house style ---------------------------------------------------------------
#
# `gates.sh` formats and re-checks `lib/` and does not look at `apps/`, so
# this source -- and the two new `lib/` modules -- would drift out of the
# house layout with nothing to notice. The same check `apps/git/test.sh` runs
# over its own directory, here over this one plus the two library files.

if out=$(
    for f in apps/ssh/*.m31 lib/sha256.m31 lib/chacha20poly1305.m31 lib/ssh.m31 \
             lib/sshkey.m31 lib/sshhosts.m31 lib/sshauth.m31 lib/sshexec.m31 lib/sshclient.m31; do
        "$LANGC" fmt --check "$f" || echo "$f"
    done 2>&1
) && [ -z "$out" ]; then
    note "source is formatted"
else
    bad "source is not formatted (run: m31c fmt <file>)" "$out"
fi

# --- SHA-256 --------------------------------------------------------------------

if build t_sha256; then
    "$WORK/t_sha256" >"$WORK/sha256.got" 2>"$WORK/sha256.err"
    python3 apps/ssh/oracle_sha256.py >"$WORK/sha256.want"
    if cmp -s "$WORK/sha256.got" "$WORK/sha256.want"; then
        note "sha256: $(wc -l <"$WORK/sha256.got") digests match hashlib"
    else
        bad "sha256" "$(diff "$WORK/sha256.got" "$WORK/sha256.want" | head -8)"
    fi
    sed 's/^/     /' "$WORK/sha256.err"
fi

# --- ChaCha20, Poly1305, and the AEAD construction ------------------------------

if ! python3 -c 'import cryptography' 2>/dev/null; then
    bad "chacha20poly1305" "the 'cryptography' package is not importable -- pip install cryptography"
else
    if build t_chacha20poly1305; then
        "$WORK/t_chacha20poly1305" >"$WORK/ccp.got" 2>"$WORK/ccp.err"
        rc=$?
        python3 apps/ssh/oracle_chacha20poly1305.py >"$WORK/ccp.want"
        if [ $rc -ne 0 ]; then
            bad "chacha20poly1305" "t_chacha20poly1305 exited $rc -- a self-check (round trip or tamper detection) trapped" \
                "$(tail -5 "$WORK/ccp.err")"
        elif cmp -s "$WORK/ccp.got" "$WORK/ccp.want"; then
            note "chacha20poly1305: $(wc -l <"$WORK/ccp.got") lines match cryptography/OpenSSL (RFC 8439 vectors, a 0-260 length sweep, a 19x19 AAD/plaintext length grid with in-process round-trip and tamper checks, and 900 further random-ish cases)"
        else
            bad "chacha20poly1305" "$(diff "$WORK/ccp.got" "$WORK/ccp.want" | head -12)"
        fi
        sed 's/^/     /' "$WORK/ccp.err"
    fi
fi

# --- openssh_block: the second ChaCha20 word layout -----------------------------

if ! python3 -c 'import cryptography' 2>/dev/null; then
    bad "chacha20_openssh" "the 'cryptography' package is not importable -- pip install cryptography"
else
    if build t_chacha20_openssh; then
        "$WORK/t_chacha20_openssh" >"$WORK/co.got" 2>"$WORK/co.err"
        rc=$?
        python3 apps/ssh/oracle_chacha20_openssh.py >"$WORK/co.want"
        if [ $rc -ne 0 ]; then
            bad "chacha20_openssh" "t_chacha20_openssh exited $rc" "$(tail -5 "$WORK/co.err")"
        elif cmp -s "$WORK/co.got" "$WORK/co.want"; then
            note "chacha20_openssh: $(wc -l <"$WORK/co.got") lines match cryptography/OpenSSL (fixed edge cases plus a 64-case sweep)"
        else
            bad "chacha20_openssh" "$(diff "$WORK/co.got" "$WORK/co.want" | head -12)"
        fi
        sed 's/^/     /' "$WORK/co.err"
    fi
fi

# --- lib/ssh.m31 Phase 2: wire-format self-checks (no oracle needed) ------------

if build t_ssh_proto; then
    out=$("$WORK/t_ssh_proto" 2>"$WORK/proto.err")
    rc=$?
    if [ $rc -eq 0 ] && [[ "$out" == *"0 failures"* ]]; then
        note "ssh wire-format self-checks: $out"
    else
        bad "ssh wire-format self-checks" "exited $rc" "$out" "$(tail -10 "$WORK/proto.err")"
    fi
fi

# --- lib/ssh.m31 Phase 2: exchange hash and key derivation ----------------------

if build t_ssh_wire; then
    "$WORK/t_ssh_wire" >"$WORK/wire.got" 2>"$WORK/wire.err"
    rc=$?
    python3 apps/ssh/oracle_ssh_wire.py >"$WORK/wire.want"
    if [ $rc -ne 0 ]; then
        bad "ssh wire (exchange hash / key derivation)" "t_ssh_wire exited $rc" "$(tail -5 "$WORK/wire.err")"
    elif cmp -s "$WORK/wire.got" "$WORK/wire.want"; then
        note "ssh wire (exchange hash / key derivation): $(wc -l <"$WORK/wire.got") lines match independent hashlib math"
    else
        bad "ssh wire (exchange hash / key derivation)" "$(diff "$WORK/wire.got" "$WORK/wire.want" | head -12)"
    fi
    sed 's/^/     /' "$WORK/wire.err"
fi

# --- lib/sshkey.m31 Phase 3: the openssh-key-v1 parser -------------------------
#
# The checks inside t_ssh_key are pure fixtures (one per Error variant, a
# prefix sweep). Its real-key half is checked against `cryptography`/OpenSSL:
# three keys made by `ssh-keygen` (different comments, one empty) must give
# the same public key, comment and (deterministic) Ed25519 signature, and a
# passphrase-protected `ssh-keygen -N` key must be refused with the message.

if build t_ssh_key; then
    if ! command -v ssh-keygen >/dev/null || ! python3 -c 'import cryptography' 2>/dev/null; then
        bad "sshkey" "ssh-keygen and the 'cryptography' package are both needed"
    else
        for k in k1 k2 k3; do
            ssh-keygen -t ed25519 -f "$WORK/$k" -N "" -q -C "$k comment"
        done
        ssh-keygen -t ed25519 -f "$WORK/k4" -N "" -q -C ""
        ssh-keygen -t ed25519 -f "$WORK/kenc" -N "a passphrase" -q -C "encrypted"
        keys=("$WORK/k1" "$WORK/k2" "$WORK/k3" "$WORK/k4")
        "$WORK/t_ssh_key" "$WORK/kenc" "${keys[@]}" >"$WORK/key.got" 2>"$WORK/key.err"
        rc=$?
        python3 apps/ssh/oracle_ssh_key.py "$WORK/kenc" "${keys[@]}" >"$WORK/key.want"
        head -1 "$WORK/key.got" >"$WORK/key.summary"
        tail -n +2 "$WORK/key.got" >"$WORK/key.lines"
        if [ $rc -ne 0 ] || ! grep -q ' checks, 0 failures$' "$WORK/key.summary"; then
            bad "sshkey" "t_ssh_key exited $rc" "$(cat "$WORK/key.summary")" "$(tail -5 "$WORK/key.err")"
        elif cmp -s "$WORK/key.lines" "$WORK/key.want"; then
            note "sshkey: $(cat "$WORK/key.summary"); 4 ssh-keygen keys match cryptography (public key, comment, signature) and the encrypted one is refused"
        else
            bad "sshkey" "$(diff "$WORK/key.lines" "$WORK/key.want" | head -12)"
        fi
    fi
fi

# --- lib/sshhosts.m31 Phase 3: known_hosts ---------------------------------------
#
# Pure fixtures inside t_ssh_hosts (including three lines from a real
# `ssh-keygen -H`). Here: a plain known_hosts and its `ssh-keygen -H` copy
# must answer identically, and both must agree with `ssh-keygen -F`, the
# OpenSSH lookup, on whether each name is listed at all.

if build t_ssh_hosts; then
    if ! command -v ssh-keygen >/dev/null; then
        bad "sshhosts" "ssh-keygen is needed"
    else
        ssh-keygen -t ed25519 -f "$WORK/ha" -N "" -q
        ssh-keygen -t ed25519 -f "$WORK/hb" -N "" -q
        ssh-keygen -t ed25519 -f "$WORK/hc" -N "" -q
        {
            printf 'oracle.example,10.9.9.9 %s\n' "$(cut -d' ' -f1,2 "$WORK/ha.pub")"
            printf '[other.example]:2222 %s\n' "$(cut -d' ' -f1,2 "$WORK/hb.pub")"
            printf '10.1.2.3 %s\n' "$(cut -d' ' -f1,2 "$WORK/ha.pub")"
            printf '[lab.example]:2200 %s\n' "$(cut -d' ' -f1,2 "$WORK/hc.pub")"
        } >"$WORK/kh_plain"
        cp "$WORK/kh_plain" "$WORK/kh_hashed"
        ssh-keygen -H -f "$WORK/kh_hashed" >/dev/null 2>&1
        "$WORK/t_ssh_hosts" "$WORK/kh_plain" "$WORK/kh_hashed" >"$WORK/hosts.got" 2>"$WORK/hosts.err"
        rc=$?
        head -1 "$WORK/hosts.got" >"$WORK/hosts.summary"
        mismatch=""
        n=0
        while read -r hp arrow ans; do
            [ "$arrow" = "->" ] || continue
            host=${hp%:*}
            port=${hp##*:}
            if [ "$port" = 22 ]; then query=$host; else query="[$host]:$port"; fi
            if ssh-keygen -F "$query" -f "$WORK/kh_plain" >/dev/null 2>&1; then want=listed; else want=unlisted; fi
            if [ "$ans" = "!3" ]; then got=unlisted; else got=listed; fi
            # ORACLE.example only tests case folding, which is not compared against
            # ssh-keygen -F (t_ssh_hosts checks it itself).
            if [ "$host" != "ORACLE.example" ] && [ "$want" != "$got" ]; then
                mismatch="$mismatch $query (ssh-keygen: $want, m31: $got)"
            fi
            n=$((n + 1))
        done <"$WORK/hosts.got"
        if [ $rc -ne 0 ] || ! grep -q ' checks, 0 failures$' "$WORK/hosts.summary"; then
            bad "sshhosts" "t_ssh_hosts exited $rc" "$(cat "$WORK/hosts.summary")" "$(tail -5 "$WORK/hosts.err")"
        elif [ -n "$mismatch" ] || [ "$n" -lt 6 ]; then
            bad "sshhosts" "disagreement with ssh-keygen -F:$mismatch" "$(cat "$WORK/hosts.got")"
        else
            note "sshhosts: $(cat "$WORK/hosts.summary"); plain and ssh-keygen -H copies agree, and agree with ssh-keygen -F on $n names"
        fi
    fi
fi

# --- lib/sshauth.m31 and lib/sshexec.m31: scripted-peer self-checks ---------------
#
# No network: byte-level layouts, one fixture per Error variant, prefix sweeps
# of every parser, and reactive in-memory peers that enforce the channel
# window and packet limits over multi-megabyte transfers. The live half is
# test_auth_exec.sh.

for t in t_ssh_auth t_ssh_exec; do
    if build "$t"; then
        out=$("$WORK/$t" 2>"$WORK/$t.err")
        rc=$?
        if [ $rc -eq 0 ] && [[ "$out" == *" checks, 0 failures" ]]; then
            note "$t: $out"
        else
            bad "$t" "exited $rc" "$out" "$(tail -10 "$WORK/$t.err")"
        fi
    fi
done

echo
if [ $fail -eq 0 ]; then
    printf '\033[32mall %d apps/ssh checks passed\033[0m\n' "$pass"
else
    printf '\033[31m%d of %d apps/ssh checks FAILED\033[0m\n' "$fail" "$((pass + fail))"
fi
exit $fail
