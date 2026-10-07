#!/usr/bin/env bash
# Every check for certificate-chain validation (`lib/x509_chain.m31`, and the
# trust modes of `lib/tls.m31`: `SystemRoots` and `CaFile`).
#
#   bash apps/tls/test_chain.sh
#
# Fixtures come from `apps/tls/chain_fixtures.py` (Python's `cryptography`): a
# throwaway CA, intermediate and leaf per case, EC P-256 / P-384 and RSA 2048,
# each case either right or wrong in one named way. Every case is run
#
#   offline   through `t_chain_verify`, which calls `x509_chain.verify` with a
#             fixed clock and says which `x509_chain.Error` came out
#   live      through `t_chain_client` against `openssl s_server` serving that
#             chain on loopback (`-tls1_3`, ChaCha20-Poly1305), which says which
#             `tls.Error` the handshake ended with. A few cases (wildcards, a
#             deep chain) are offline only: the name verified is the host that
#             is connected to, and that is `localhost`.
#
# Then, against the machine's own trust store and the network (each part skipped,
# never failed, when there is no network):
#
#   the roots     the self-signature of every RSA and ECDSA root in the system
#                 bundle, with counts
#   real hosts    github.com, codeload.github.com, dev.meghraj.uk by
#                 `Trust.SystemRoots`, and an https HEAD of github.com over
#                 `lib/http.m31`, and the same host against a bundle that does
#                 not hold its root, which must be refused
#
# Nothing here switches verification off; there is no such switch.
#
# The compiler is `LANGC` (default `./target/debug/m31c`, built with
# `cargo build`).
set -uo pipefail
cd "$(dirname "$0")/../.."
. ./runtime/arch.sh

LANGC=${LANGC:-./target/debug/m31c}
WORK=$(mktemp -d)
pids=()
cleanup() {
    for pid in ${pids[@]+"${pids[@]}"}; do
        kill "$pid" 2>/dev/null
        wait "$pid" 2>/dev/null
    done
    rm -rf "$WORK"
}
trap cleanup EXIT
pass=0
fail=0

note() { printf '\033[32mok\033[0m   %s\n' "$1"; pass=$((pass + 1)); }
bad()  { printf '\033[31mFAIL\033[0m %s\n' "$1"; shift; printf '%s\n' "$@" | sed 's/^/     /'; fail=$((fail + 1)); }
skip() { printf 'skip %s\n' "$1"; }

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

finish() {
    echo
    if [ $fail -eq 0 ]; then
        printf '\033[32mall %d chain checks passed\033[0m\n' "$pass"
    else
        printf '\033[31m%d of %d chain checks FAILED\033[0m\n' "$fail" "$((pass + fail))"
    fi
    exit $fail
}

# Poll until `grep -q $2 $1` holds, the process $3 dies, or about 20 seconds pass.
await_line() {
    local file=$1 pattern=$2 pid=$3 tries=0
    until grep -q "$pattern" "$file" 2>/dev/null; do
        kill -0 "$pid" 2>/dev/null || return 1
        tries=$((tries + 1))
        [ $tries -le 400 ] || return 1
        python3 -c 'import time; time.sleep(0.05)'
    done
}

# --- house style ---------------------------------------------------------------

if out=$(
    for f in lib/tls.m31 lib/x509_chain.m31 apps/tls/t_chain_*.m31; do
        "$LANGC" fmt --check "$f" || echo "$f"
    done 2>&1
) && [ -z "$out" ]; then
    note "source is formatted"
else
    bad "source is not formatted (run: m31c fmt <file>)" "$out"
fi

build t_chain_verify && build t_chain_client && build t_chain_roots || finish
VERIFY=$WORK/t_chain_verify
CLIENT=$WORK/t_chain_client
ROOTS=$WORK/t_chain_roots

for tool in openssl python3; do
    command -v $tool >/dev/null || { bad "$tool is not installed"; finish; }
done
if ! python3 -c 'import cryptography' 2>/dev/null; then
    bad "fixtures" "the 'cryptography' package is not importable -- pip install cryptography"
    finish
fi
FX=$WORK/fx
if ! python3 apps/tls/chain_fixtures.py "$FX" >"$WORK/fixtures.log" 2>&1; then
    bad "fixtures" "$(tail -5 "$WORK/fixtures.log")"
    finish
fi
NOW=$(cat "$FX/now")
note "fixtures: $(wc -l <"$FX/cases.tsv") cases"

# --- offline: x509_chain.verify -----------------------------------------------------

offline_ok=0
while IFS=$'\t' read -r name host want mode; do
    got=$(timeout 60 "$VERIFY" "$FX/$name/anchors.pem" "$FX/$name/chain.pem" "$host" "$NOW" 2>"$WORK/verify.err")
    if [ "$want" = ok ]; then
        if grep -Eq '^ok [0-9]+$' <<<"$got"; then
            offline_ok=$((offline_ok + 1))
            note "offline $name: valid ($got)"
        else
            bad "offline $name" "wanted ok, got: $got" "$(head -3 "$WORK/verify.err")"
        fi
    elif [ "$got" = "error $want" ]; then
        note "offline $name: refused, $want"
    else
        bad "offline $name" "wanted error $want, got: $got" "$(head -3 "$WORK/verify.err")"
    fi
done <"$FX/cases.tsv"

# --- offline: depth ------------------------------------------------------------------

depth=$(timeout 60 "$VERIFY" "$FX/ok_deep_chain/anchors.pem" "$FX/ok_deep_chain/chain.pem" localhost "$NOW" 2>&1)
if [ "$depth" = "ok 10" ]; then
    note "offline a path of exactly MAXIMUM_PATH_LENGTH certificates is accepted"
else
    bad "offline ok_deep_chain depth" "wanted 'ok 10', got: $depth"
fi

# --- live: the handshake against openssl s_server -------------------------------------

# The tls.Error that a handshake must end with for each x509_chain.Error.
live_error() {
    case $1 in
        Expired) echo CertificateExpired ;;
        NotYetValid) echo CertificateNotYetValid ;;
        HostnameMismatch) echo HostnameMismatch ;;
        UnknownAuthority) echo UnknownAuthority ;;
        NotCertificateAuthority | PathLengthExceeded | WrongPurpose | BadSignature | TooDeep) echo InvalidChain ;;
        WeakAlgorithm) echo WeakCertificate ;;
        LeafUnparseable) echo BadCertificate ;;
        *) echo "?$1" ;;
    esac
}

# serve <case>: s_server on an ephemeral port, serving that case's chain to one
# client; sets SERVER_PORT and SERVER_LOG. The server's chain is leaf.pem as its
# certificate and rest.pem as everything it sends after, in the order sent.
serve() {
    local name=$1
    SERVER_LOG=$WORK/server.$name.log
    local chain_option=()
    [ -s "$FX/$name/rest.pem" ] && chain_option=(-cert_chain "$FX/$name/rest.pem")
    openssl s_server -accept 0 -naccept 1 -www \
        -cert "$FX/$name/leaf.pem" -key "$FX/$name/key.pem" ${chain_option[@]+"${chain_option[@]}"} \
        -cipher 'ALL:@SECLEVEL=0' -ciphersuites TLS_CHACHA20_POLY1305_SHA256 -tls1_3 \
        >"$SERVER_LOG" 2>"$WORK/server.$name.err" </dev/null &
    SERVER_PID=$!
    pids+=("$SERVER_PID")
    await_line "$SERVER_LOG" '^ACCEPT ' "$SERVER_PID" || return 1
    SERVER_PORT=$(sed -n 's/^ACCEPT .*:\([0-9][0-9]*\)$/\1/p' "$SERVER_LOG" | head -1)
    [ -n "$SERVER_PORT" ]
}

# live <case> <expected output, one line> [trust, default the case's anchors] [now]
#      [a regex the server's own error log must match]
# The host is the case's own (`localhost`, or a `*.localhost` name).
live() {
    local name=$1 want=$2 trust=${3:-$FX/$name/anchors.pem} now=${4:-$NOW} server_says=${5:-}
    local host
    host=$(awk -F'\t' -v name="$name" '$1 == name { print $2 }' "$FX/cases.tsv")
    if ! getent hosts "$host" >/dev/null 2>&1; then
        skip "live $name: $host does not resolve here"
        return
    fi
    if ! serve "$name"; then
        bad "live $name" "s_server did not start" "$(head -3 "$WORK/server.$name.err")"
        return
    fi
    local out rc
    out=$(timeout 60 "$CLIENT" handshake "$host" "$SERVER_PORT" "$trust" "$now" 2>"$WORK/client.$name.err" | tr '\n' ' ')
    rc=$?
    kill "$SERVER_PID" 2>/dev/null
    wait "$SERVER_PID" 2>/dev/null
    if [ "$out" = "$want " ] && [ -n "$server_says" ] && ! grep -Eq "$server_says" "$WORK/server.$name.err"; then
        bad "live $name" "the server's log did not match /$server_says/" "$(head -3 "$WORK/server.$name.err")"
    elif [ "$out" = "$want " ]; then
        note "live $name: $want"
    else
        bad "live $name" "wanted '$want', got '$out'" "$(head -3 "$WORK/client.$name.err")"
    fi
}

while IFS=$'\t' read -r name host want mode; do
    [ "$mode" = live ] || continue
    if [ "$want" = ok ]; then
        live "$name" "connected closed"
    elif [ "$name" = bad_weak_hash_sha1_leaf ]; then
        # The client's signature_algorithms_cert leaves SHA-1 out, and OpenSSL
        # honours it: it will not send this leaf, and says so with an alert.
        live "$name" "error PeerAlert" "$FX/$name/anchors.pem" "$NOW" 'no suitable signature algorithm'
    else
        live "$name" "error $(live_error "$want")"
    fi
done <"$FX/cases.tsv"

# The clock the client is given is the clock it judges by, and by default that
# is the machine's.
live ok_ecdsa_p256 "connected closed" "$FX/ok_ecdsa_p256/anchors.pem" real
live bad_leaf_expired "error CertificateExpired" "$FX/bad_leaf_expired/anchors.pem" real

# --- trust sources -----------------------------------------------------------------------

# SystemRoots reads $SSL_CERT_FILE first, and means only that file.
SSL_CERT_FILE=$FX/ok_rsa/anchors.pem serve ok_rsa
out=$(SSL_CERT_FILE=$FX/ok_rsa/anchors.pem timeout 60 "$CLIENT" handshake localhost "$SERVER_PORT" system "$NOW" 2>&1 | tr '\n' ' ')
kill "$SERVER_PID" 2>/dev/null; wait "$SERVER_PID" 2>/dev/null
[ "$out" = "connected closed " ] && note "SystemRoots reads \$SSL_CERT_FILE" || bad "SystemRoots reads \$SSL_CERT_FILE" "got '$out'"

serve ok_rsa
out=$(SSL_CERT_FILE=$FX/ok_ecdsa_p256/anchors.pem timeout 60 "$CLIENT" handshake localhost "$SERVER_PORT" system "$NOW" 2>&1 | tr '\n' ' ')
kill "$SERVER_PID" 2>/dev/null; wait "$SERVER_PID" 2>/dev/null
[ "$out" = "error UnknownAuthority " ] && note "SystemRoots with \$SSL_CERT_FILE trusts only that file" || bad "SystemRoots with \$SSL_CERT_FILE trusts only that file" "got '$out'"

out=$(SSL_CERT_FILE=$WORK/no-such-bundle.pem timeout 60 "$CLIENT" handshake localhost 9 system "$NOW" 2>&1 | tr '\n' ' ')
[ "$out" = "error TrustStore " ] && note "an unreadable \$SSL_CERT_FILE is TrustStore, with no fallback to the system's bundle" || bad "unreadable \$SSL_CERT_FILE" "got '$out'"

printf 'not a certificate\n' >"$WORK/empty.pem"
out=$(timeout 60 "$CLIENT" handshake localhost 9 "$WORK/empty.pem" "$NOW" 2>&1 | tr '\n' ' ')
[ "$out" = "error TrustStore " ] && note "a CaFile with no certificate is TrustStore" || bad "CaFile with no certificate" "got '$out'"

out=$(timeout 60 "$CLIENT" handshake localhost 9 "$WORK/no-such-bundle.pem" "$NOW" 2>&1 | tr '\n' ' ')
[ "$out" = "error TrustStore " ] && note "a missing CaFile is TrustStore" || bad "missing CaFile" "got '$out'"

# The same bytes in the roots file twice, and a certificate that is no root, are harmless.
cat "$FX/ok_rsa/anchors.pem" "$FX/ok_rsa/anchors.pem" "$FX/ok_rsa/leaf.pem" >"$WORK/noisy.pem"
live ok_rsa "connected closed" "$WORK/noisy.pem"

# --- the system's roots --------------------------------------------------------------------

bundle=
if [ -n "${SSL_CERT_FILE:-}" ]; then
    bundle=$SSL_CERT_FILE
else
    for candidate in /etc/pki/tls/certs/ca-bundle.crt /etc/ssl/certs/ca-certificates.crt /etc/ssl/cert.pem; do
        [ -r "$candidate" ] && { bundle=$candidate; break; }
    done
fi
if [ -z "$bundle" ]; then
    skip "system roots: no CA bundle on this machine"
else
    out=$(timeout 120 "$ROOTS" "$bundle" 2>&1)
    rc=$?
    summary=$(grep -E '^(roots|verified) ' <<<"$out" | tr '\n' ';' | sed 's/;$//; s/;/; /')
    expected=$(grep -c 'BEGIN CERTIFICATE' "$bundle")
    # An independent count of the roots whose own signature is MD5 or SHA-1, by openssl.
    weak_expected=$(python3 -W ignore - "$bundle" <<'PY'
import sys, re
from cryptography import x509
text = open(sys.argv[1]).read()
weak = 0
for pem in re.findall(r'-----BEGIN CERTIFICATE-----.*?-----END CERTIFICATE-----', text, re.S):
    try:
        cert = x509.load_pem_x509_certificate(pem.encode())
        name = cert.signature_algorithm_oid._name
    except Exception:
        continue
    if 'sha1' in name.lower() or 'md5' in name.lower() or 'md2' in name.lower():
        weak += 1
print(weak)
PY
)
    parsed=$(sed -n 's/^roots \([0-9]*\) skipped.*/\1/p' <<<"$out")
    skipped=$(sed -n 's/^roots [0-9]* skipped \([0-9]*\) .*/\1/p' <<<"$out")
    weak=$(sed -n 's/^verified [0-9]* weak \([0-9]*\) .*/\1/p' <<<"$out")
    if [ $rc -ne 0 ]; then
        bad "system roots: a root's own signature did not verify" "$(grep -v '^self .*too weak' <<<"$out" | head -8)"
    elif [ "$((parsed + skipped))" -ne "$expected" ]; then
        bad "system roots: $parsed parsed and $skipped skipped, but the bundle holds $expected" "$summary"
    elif [ "$weak" != "$weak_expected" ]; then
        bad "system roots: $weak weak self-signatures, but python's cryptography counts $weak_expected" "$summary"
    else
        note "system roots ($bundle): every RSA and ECDSA self-signature verifies -- $summary"
    fi
fi

# --- real hosts ----------------------------------------------------------------------------

have_network() { timeout 8 getent hosts "$1" >/dev/null 2>&1; }

# real <host> : Trust.SystemRoots against a live server, skipped when it cannot be reached.
real() {
    local host=$1 out rc
    if ! have_network "$host"; then
        skip "$host: no network"
        return
    fi
    out=$(timeout 90 "$CLIENT" handshake "$host" 443 system real 2>"$WORK/real.$host.err" | tr '\n' ' ')
    case "$out" in
        "connected closed ") note "$host: handshake verified by SystemRoots" ;;
        "error Connect ") skip "$host: could not connect" ;;
        *) bad "$host" "got '$out'" "$(head -3 "$WORK/real.$host.err")" ;;
    esac
}
real github.com
real codeload.github.com
real dev.meghraj.uk

if have_network github.com; then
    out=$(timeout 90 "$CLIENT" get github.com 443 system real / 2>"$WORK/http.err" | tr '\n' ' ')
    if grep -Eq '^connected status [0-9]+ closed $' <<<"$out"; then
        note "github.com: an https HEAD over lib/http.m31, $(grep -o 'status [0-9]*' <<<"$out")"
    else
        bad "github.com: https HEAD" "got '$out'" "$(head -3 "$WORK/http.err")"
    fi
    out=$(timeout 90 "$CLIENT" handshake github.com 443 "$FX/ok_ecdsa_p256/anchors.pem" real 2>&1 | tr '\n' ' ')
    case "$out" in
        "error UnknownAuthority ") note "github.com is refused when its root is not in the bundle" ;;
        "error Connect ") skip "github.com: could not connect" ;;
        *) bad "github.com against a bundle without its root" "wanted UnknownAuthority, got '$out'" ;;
    esac
else
    skip "github.com https HEAD: no network"
fi

finish
