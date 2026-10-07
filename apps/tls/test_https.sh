#!/usr/bin/env bash
# `https://` through `lib/http.m31`: `http.get` and `http.fetch` over a verified
# TLS connection.
#
#   bash apps/tls/test_https.sh
#
# Against a loopback server (`apps/tls/https_server.py`, TLS 1.3 only, with a
# plain HTTP server beside it) holding a certificate from
# `apps/tls/chain_fixtures.py`:
#
#   the happy path   a body with a length, chunked, empty, large (300 kB), a
#                    redirect within the origin, and an `http` URL on the plain
#                    port, which must still work
#   trust            a CA file that holds the issuer, one that does not
#                    (`CertificateUntrusted`), one that is not there
#                    (`TlsFailed`), a right pin, a wrong pin
#   identity         a certificate for another name (`HostnameMismatch`), an
#                    IP literal (the certificate has no such name), an expired
#                    certificate (`CertificateExpired`)
#   redirects        `http` to `https` on the same host is followed; `https`
#                    to `http` is `InsecureRedirect`; another host is `BadUrl`
#   URLs             the default port of each scheme, from `parse_url`
#
# Then, skipped (never failed) when there is no network, a real server: an
# `https` GET of example.com, and an `http` URL that redirects to `https`.
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

finish() {
    echo
    if [ $fail -eq 0 ]; then
        printf '\033[32mall %d https checks passed\033[0m\n' "$pass"
    else
        printf '\033[31m%d of %d https checks FAILED\033[0m\n' "$fail" "$((pass + fail))"
    fi
    exit $fail
}

build() {
    local name=$1
    if ! "$LANGC" --emit-c "apps/tls/$name.m31" -o "$WORK/$name.c" 2>"$WORK/$name.diag"; then
        bad "compile $name" "$(grep -v '^warning' "$WORK/$name.diag" | head -5)"
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

await_line() {
    local file=$1 pattern=$2 pid=$3 tries=0
    until grep -q "$pattern" "$file" 2>/dev/null; do
        kill -0 "$pid" 2>/dev/null || return 1
        tries=$((tries + 1))
        [ $tries -le 400 ] || return 1
        python3 -c 'import time; time.sleep(0.05)'
    done
}

for tool in openssl python3; do
    command -v $tool >/dev/null || { bad "$tool is not installed"; finish; }
done
if ! python3 -c 'import cryptography' 2>/dev/null; then
    skip "python3 'cryptography' is not installed: no fixtures"
    finish
fi

# --- house style -------------------------------------------------------------

if out=$(
    for f in lib/http.m31 apps/tls/t_https_client.m31; do
        "$LANGC" fmt --check "$f" || echo "$f"
    done 2>&1
) && [ -z "$out" ]; then
    note "source is formatted"
else
    bad "source is not formatted (run: m31c fmt <file>)" "$out"
fi

build t_https_client || finish
CLIENT=$WORK/t_https_client

FX=$WORK/fx
if ! python3 apps/tls/chain_fixtures.py "$FX" >"$WORK/fixtures.log" 2>&1; then
    bad "fixtures" "$(tail -5 "$WORK/fixtures.log")"
    finish
fi

# serve <case>: sets TLS_PORT and PLAIN_PORT.
serve() {
    local name=$1
    SERVER_LOG=$WORK/server.$name.log
    local rest=()
    [ -s "$FX/$name/rest.pem" ] && rest=("$FX/$name/rest.pem")
    python3 apps/tls/https_server.py "$FX/$name/leaf.pem" "$FX/$name/key.pem" ${rest[@]+"${rest[@]}"} \
        >"$SERVER_LOG" 2>"$WORK/server.$name.err" </dev/null &
    SERVER_PID=$!
    pids+=("$SERVER_PID")
    await_line "$SERVER_LOG" '^ready ' "$SERVER_PID" || return 1
    read -r _ TLS_PORT PLAIN_PORT <"$SERVER_LOG"
}

# check <label> <expected output, lines joined by '|'> <client arguments...>
check() {
    local label=$1 want=$2
    shift 2
    local got
    got=$(timeout 60 "$CLIENT" "$@" 2>"$WORK/client.err" | tr '\n' '|')
    got=${got%|}
    if [ "$got" = "$want" ]; then
        note "$label"
    else
        bad "$label" "wanted: $want" "got:    $got" "$(head -3 "$WORK/client.err")"
    fi
}

# --- URLs, with no network ------------------------------------------------------

check "parse: https gets port 443, which the Host field leaves out" \
    'https example.com 443 example.com https://example.com/' parse https://example.com
check "parse: http keeps port 80" \
    'http example.com 80 example.com http://example.com/' parse http://example.com
check "parse: https on another port says so" \
    'https example.com 8443 example.com:8443 https://example.com:8443/a' parse https://example.com:8443/a
check "parse: port 80 is not the https default" \
    'https example.com 80 example.com:80 https://example.com:80/' parse https://example.com:80/
check "parse: port 443 is not the http default" \
    'http example.com 443 example.com:443 http://example.com:443/' parse http://example.com:443/
check "parse: an upper-case scheme and an IPv6 literal" \
    'https ::1 443 [::1] https://[::1]/p' parse HTTPS://[::1]/p
check "parse: another scheme is still refused" \
    'error BadUrl' parse ftp://example.com/
check "parse: userinfo is still refused over https" \
    'error BadUrl' parse https://user:pw@example.com/

# --- the happy path -----------------------------------------------------------------

CASE=ok_ecdsa_p256
if ! serve $CASE; then
    bad "server for $CASE did not start" "$(head -3 "$WORK/server.$CASE.err")"
    finish
fi
CA=$FX/$CASE/anchors.pem
H=https://localhost:$TLS_PORT
P=http://localhost:$PLAIN_PORT

check "https GET with a Content-Length body" 'status 200|body 15|text hello over tls' "$H/hello" "$CA"
check "https GET, 204 with no body" 'status 204|body 0|text ' "$H/empty" "$CA"
check "https GET, a chunked body" 'status 200|body 10|text 0123456789' "$H/chunked" "$CA"
check "https GET, 300 kB in many records" "status 200|body 300000|text $(printf 'x%.0s' {1..60})" "$H/big" "$CA"
check "https GET, 404 is a response and not an error" 'status 404|body 14|text no such route' "$H/missing" "$CA"
check "https HEAD, no body" 'status 200|body 0|text ' "$H/hello" "$CA" 5 HEAD
check "the Host field names the port when it is not 443" "status 200|body 20|text localhost:$TLS_PORT GET" "$H/echo" "$CA"
check "a redirect within the https origin is followed" 'status 200|body 15|text hello over tls' "$H/same" "$CA"
check "redirects: 0 hands back the 302" 'status 302|body 0|text ' "$H/same" "$CA" 0
check "a redirect loop stops at the limit" 'status 302|body 0|text ' "$H/loop" "$CA"
check "the same client still speaks plain http" 'status 200|body 17|text hello over plain' "$P/hello" "$CA"

# --- redirects across schemes -----------------------------------------------------------

check "http to https on the same host is followed" 'status 200|body 15|text hello over tls' "$P/upgrade" "$CA"
check "https to http is refused, not followed" 'error InsecureRedirect' "$H/to-http" "$CA"
check "https to http is not the 302 with redirects: 0" 'status 302|body 0|text ' "$H/to-http" "$CA" 0
check "a redirect to another host is refused" 'error BadUrl' "$H/elsewhere" "$CA"

# --- trust ---------------------------------------------------------------------------------

check "a CA file that does not hold the issuer is refused" 'error CertificateUntrusted' "$H/hello" "$FX/bad_untrusted_root/anchors.pem"
check "a CA file that is not there is a TLS failure" 'error TlsFailed' "$H/hello" "$WORK/no-such-file.pem"
check "the system roots do not hold a throwaway CA" 'error CertificateUntrusted' "$H/hello" system

pin=$(openssl x509 -in "$FX/$CASE/leaf.pem" -pubkey -noout | openssl pkey -pubin -outform DER | openssl dgst -sha256 -binary | od -An -v -tx1 | tr -d ' \n')
check "the right pin is accepted, with no CA at all" 'status 200|body 15|text hello over tls' "$H/hello" "pin:$pin"
check "a wrong pin is refused" 'error CertificateUntrusted' "$H/hello" "pin:$(printf '0%.0s' {1..64})"
check "https to an IP literal: the certificate has no such name" 'error HostnameMismatch' "https://[::1]:$TLS_PORT/hello" "$CA"

# --- a second key type -----------------------------------------------------------------------

CASE=ok_rsa
if serve $CASE; then
    check "an RSA certificate chain" 'status 200|body 15|text hello over tls' "https://localhost:$TLS_PORT/hello" "$FX/$CASE/anchors.pem"
else
    bad "server for $CASE did not start" "$(head -3 "$WORK/server.$CASE.err")"
fi

# --- identity ---------------------------------------------------------------------------------

for pair in bad_leaf_expired:CertificateExpired bad_hostname:HostnameMismatch bad_self_signed_leaf:CertificateUntrusted; do
    CASE=${pair%%:*}
    want=${pair##*:}
    if serve "$CASE"; then
        check "$CASE: $want" "error $want" "https://localhost:$TLS_PORT/hello" "$FX/$CASE/anchors.pem"
    else
        bad "server for $CASE did not start" "$(head -3 "$WORK/server.$CASE.err")"
    fi
done

# --- a server that does not speak TLS ------------------------------------------------------------

CASE=ok_ecdsa_p256
if serve $CASE; then
    check "https to a plain-HTTP port is a TLS failure, never a plaintext request" 'error TlsFailed' "https://localhost:$PLAIN_PORT/hello" "$FX/$CASE/anchors.pem"
fi

# --- a real server ------------------------------------------------------------------------------

have_network() { timeout 8 getent hosts "$1" >/dev/null 2>&1; }

if have_network example.com; then
    out=$(timeout 60 "$CLIENT" https://example.com/ system 2>"$WORK/real.err" | tr '\n' '|')
    case "$out" in
        'status 200|body '*) note "example.com: an https GET, verified by the system roots" ;;
        *) bad "example.com: https GET" "got '$out'" "$(head -3 "$WORK/real.err")" ;;
    esac
else
    skip "example.com: no network"
fi
if have_network github.com; then
    out=$(timeout 60 "$CLIENT" http://github.com/ system 2>"$WORK/real.err" | head -1)
    if [ "$out" = 'status 200' ]; then
        note "github.com: an http URL that redirects to https is followed"
    else
        bad "github.com: http to https" "got '$out'" "$(head -3 "$WORK/real.err")"
    fi
    out=$(timeout 60 "$CLIENT" https://github.com/ "$FX/ok_ecdsa_p256/anchors.pem" 2>&1 | head -1)
    if [ "$out" = 'error CertificateUntrusted' ]; then
        note "github.com is refused when its root is not in the bundle"
    else
        bad "github.com against a bundle without its root" "wanted CertificateUntrusted, got '$out'"
    fi
else
    skip "github.com: no network"
fi

finish
