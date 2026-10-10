#!/usr/bin/env bash
# Every check for `lib/encoding/der.m31` and `lib/pki/x509.m31` (TLS milestone M3): no
# network, no clock, nothing wired into another app.
#
#   bash apps/tls/test_x509.sh
#
# Nothing here compares the parser with itself. The oracles are Python's
# `cryptography` package (OpenSSL underneath) and `openssl x509 -text`:
#
#   - `x509_dump` prints every field of every certificate, and the output must
#     be byte-identical to `x509_oracle.py dump` over: the three committed
#     fixture chains, the certificates `x509_make_certs.py` synthesizes
#     (RSA, P-256, P-384, wildcard and IP SANs, CA constraints, usages), and
#     every certificate in the system CA bundle;
#   - `x509_oracle.py openssl-check` cross-checks serial, validity, SAN and
#     basicConstraints against `openssl x509` on the same files;
#   - `der_selfcheck` is hand-built DER vectors (BER, non-minimal and absurd
#     lengths, 10000-deep nesting, INTEGER/OID/time edge cases);
#   - `x509_cases` is the malformed-certificate, hostname, key-usage, validity,
#     PEM and mutation-fuzz tables; every expected outcome is written down
#     in `x509_make_certs.py` from RFC 5280, not computed by the parser.
#
# `pip install cryptography` first. Run from anywhere; needs `cargo build`.
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

# --- house style ---------------------------------------------------------------

if out=$(
    for f in lib/encoding/der.m31 lib/pki/x509.m31 apps/tls/der_*.m31 apps/tls/x509_*.m31; do
        "$LANGC" fmt --check "$f" >/dev/null 2>&1 || echo "$f"
    done
) && [ -z "$out" ]; then
    note "der/x509 sources are formatted (m31c fmt --check)"
else
    bad "source is not formatted (run: m31c fmt <file>)" "$out"
fi

if ! python3 -c 'import cryptography' 2>/dev/null; then
    bad "python3 cryptography is missing" "pip install cryptography"
    echo
    printf '\033[31m%d of %d apps/tls x509 checks FAILED\033[0m\n' "$fail" "$((pass + fail))"
    exit $fail
fi

built=1
for program in der_selfcheck x509_cases x509_dump; do
    build "$program" || built=0
done

if [ $built -eq 1 ]; then
    # --- der: hand-built vectors ----------------------------------------------
    out=$("$WORK/der_selfcheck" 2>"$WORK/der.err"); rc=$?
    if [ $rc -eq 0 ] && [[ "$out" == *" 0 failures" ]]; then
        note "der_selfcheck: $out"
    else
        bad "der_selfcheck" "exited $rc" "$out" "$(tail -5 "$WORK/der.err")"
    fi

    # --- synthesized certificates ---------------------------------------------
    GEN="$WORK/gen"
    if ! python3 apps/tls/x509_make_certs.py "$GEN" >"$WORK/gen.out" 2>"$WORK/gen.err"; then
        bad "x509_make_certs.py" "$(tail -5 "$WORK/gen.err")"
    else
        note "$(cat "$WORK/gen.out" | sed 's|'"$GEN"'|<gen>|')"

        out=$("$WORK/x509_cases" "$GEN" apps/tls/x509_certs 2>"$WORK/cases.err"); rc=$?
        if [ $rc -eq 0 ] && [[ "$out" == *" 0 failures" ]]; then
            note "x509_cases: $out"
        else
            bad "x509_cases" "exited $rc" "$(printf '%s\n' "$out" | head -20)" "$(tail -5 "$WORK/cases.err")"
        fi
    fi

    # --- field-for-field against the oracle ------------------------------------
    # compare <label> <pem file> [anchors]
    compare() {
        local label=$1 file=$2 mode=${3:-}
        "$WORK/x509_dump" "$file" $mode >"$WORK/m31.dump" 2>"$WORK/m31.err"
        python3 apps/tls/x509_oracle.py dump "$file" >"$WORK/py.dump" 2>"$WORK/py.err"
        local expected got
        expected=$(grep -c -- '-----BEGIN CERTIFICATE-----' "$file")
        got=$(grep -c '^cert [0-9]*$' "$WORK/m31.dump")
        if grep -q 'ERROR' "$WORK/m31.dump"; then
            bad "$label: the parser refused a certificate" "$(grep 'ERROR' "$WORK/m31.dump" | head -5)"
        elif [ "$got" != "$expected" ]; then
            bad "$label: parsed $got of $expected certificates" "$(tail -3 "$WORK/m31.err")"
        elif cmp -s "$WORK/m31.dump" "$WORK/py.dump"; then
            note "$label: $got certificates, $(wc -l <"$WORK/m31.dump") field lines identical to cryptography"
        else
            bad "$label: dump differs from cryptography" "$(diff "$WORK/m31.dump" "$WORK/py.dump" | head -12)"
        fi
    }

    for chain in github.com codeload.github.com dev.meghraj.uk; do
        compare "fixture $chain" "apps/tls/x509_certs/$chain.pem"
    done

    if [ -d "$GEN" ]; then
        cat "$GEN"/good_*.pem "$GEN"/names_*.pem >"$WORK/synthesized.pem"
        compare "synthesized certificates" "$WORK/synthesized.pem"
    fi

    bundle=""
    for candidate in "${SYSTEM_CA_BUNDLE:-}" /etc/pki/tls/certs/ca-bundle.crt /etc/ssl/certs/ca-certificates.crt; do
        if [ -n "$candidate" ] && [ -f "$candidate" ]; then bundle=$candidate; break; fi
    done
    if [ -n "$bundle" ]; then
        compare "system CA bundle ($bundle, trust-anchor mode)" "$bundle" anchors
        compare "system CA bundle ($bundle, strict mode)" "$bundle"
    else
        printf 'skip system CA bundle: none found (set SYSTEM_CA_BUNDLE)\n'
    fi

    # --- against openssl x509 --------------------------------------------------
    if command -v openssl >/dev/null 2>&1; then
        for file in apps/tls/x509_certs/github.com.pem apps/tls/x509_certs/codeload.github.com.pem \
                    apps/tls/x509_certs/dev.meghraj.uk.pem ${bundle:+"$bundle"}; do
            if out=$(python3 apps/tls/x509_oracle.py openssl-check "$file" 2>&1); then
                note "openssl x509 agrees on $(basename "$file"): ${out#openssl cross-check: }"
            else
                bad "openssl x509 disagrees on $(basename "$file")" "$(printf '%s\n' "$out" | head -8)"
            fi
        done
    else
        printf 'skip openssl cross-check: no openssl on PATH\n'
    fi
fi

echo
if [ $fail -eq 0 ]; then
    printf '\033[32mall %d apps/tls x509 checks passed\033[0m\n' "$pass"
else
    printf '\033[31m%d of %d apps/tls x509 checks FAILED\033[0m\n' "$fail" "$((pass + fail))"
fi
exit $fail
