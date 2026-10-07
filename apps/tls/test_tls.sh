#!/usr/bin/env bash
# Every check for the TLS 1.3 client handshake (`lib/tls.m31`), against peers
# that are not this repository. Modelled on `apps/tls/test_tls13.sh`.
#
#   bash apps/tls/test_tls.sh
#
# Three kinds of peer, all on loopback, all disposable (a temporary directory,
# an ephemeral port, a readiness poll, cleanup on exit; no bare sleeps):
#
#   openssl s_server    the real thing: `-tls1_3 -ciphersuites
#                       TLS_CHACHA20_POLY1305_SHA256` with a throwaway P-256
#                       certificate. An HTTP GET through `lib/http.m31`
#                       (`-www`), an echo (`-rev`), the wrong pin, a TLS 1.2
#                       only server.
#   oracle_tls_server   a TLS 1.3 server written from RFC 8446 on Python's
#                       `cryptography` package that misbehaves on request:
#                       a bad Finished, a bad CertificateVerify, a
#                       HelloRetryRequest, the downgrade sentinel, a close
#                       without close_notify, KeyUpdate, ... Every way the
#                       client must refuse is here with the error it must give.
#   github.com          skipped, never failed, without a network: a real
#                       server, with the pin taken through openssl. Interop only.
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

# matches <regex>: does all of stdin match it? (`^` and `$` are the ends of the text.)
matches() { perl -0777 -e 'local $/; my $text = <STDIN>; exit($text =~ /$ARGV[0]/ ? 0 : 1)' "$1"; }

# --- house style ---------------------------------------------------------------

if out=$(
    for f in lib/tls.m31 apps/tls/t_tls_*.m31; do
        "$LANGC" fmt --check "$f" || echo "$f"
    done 2>&1
) && [ -z "$out" ]; then
    note "source is formatted"
else
    bad "source is not formatted (run: m31c fmt <file>)" "$out"
fi

build t_tls_client || { echo; printf '\033[31m%d of %d tls checks FAILED\033[0m\n' "$fail" "$((pass + fail))"; exit 1; }
CLIENT=$WORK/t_tls_client

# --- the oracle matrix -----------------------------------------------------------

# oracle <name> <oracle mode> <curve> <client mode> <client argument> <host>
#        <expected client output, as a regex over the whole of it>
#        <expected client exit> <expected oracle log regex, or ->
oracle() {
    local name=$1 mode=$2 curve=$3 client_mode=$4 argument=$5 host=$6 want=$7 want_rc=$8 want_log=$9
    local log=$WORK/oracle.$name.log out=$WORK/client.$name.out
    python3 apps/tls/oracle_tls_server.py "$mode" "$curve" "${ORACLE_BIND:-127.0.0.1}" >"$log" 2>"$WORK/oracle.$name.err" &
    local pid=$!
    pids+=("$pid")
    if ! await_line "$log" '^READY ' "$pid"; then
        bad "$name" "the oracle did not start" "$(tail -5 "$WORK/oracle.$name.err")"
        return
    fi
    local port pin
    read -r _ port pin <"$log"
    pin=${PIN_OVERRIDE:-$pin}
    timeout 60 "$CLIENT" "$client_mode" "$host" "$port" "$pin" "$argument" >"$out" 2>"$WORK/client.$name.err"
    local rc=$?
    wait "$pid" 2>/dev/null
    local text
    text=$(cat "$out")
    if [ "$rc" -ne "$want_rc" ]; then
        bad "$name" "client exited $rc, wanted $want_rc" "output: $text" "$(tail -3 "$WORK/client.$name.err")" "oracle: $(tail -4 "$log")"
    elif ! matches "$want" <<<"$text"; then
        bad "$name" "output did not match /$want/" "got: $text" "oracle: $(tail -4 "$log")"
    elif [ "$want_log" != - ] && ! grep -Eq "$want_log" "$log"; then
        bad "$name" "the oracle's log did not match /$want_log/" "$(tail -8 "$log")"
    elif grep -q 'server error' "$log"; then
        bad "$name" "the oracle itself failed" "$(grep 'server error' "$log")"
    else
        note "$name"
    fi
}

if ! python3 -c 'import cryptography' 2>/dev/null; then
    bad "oracle" "the 'cryptography' package is not importable -- pip install cryptography"
else
    LOCAL=127.0.0.1
    # The handshakes that are right.
    oracle ok_p256 ok p256 echo hello $LOCAL '^line echo: hello\nclosed$' 0 'client_finished ok'
    oracle ok_p384 ok p384 echo hello $LOCAL '^line echo: hello\nclosed$' 0 'client_finished ok'
    oracle offers ok p256 handshake '' $LOCAL '^connected\nclosed$' 0 \
        'offered suites=1303 groups=001d versions=0304 sigalgs=0403,0503 extensions='
    ORACLE_BIND=::1 oracle sni ok p256 handshake '' localhost '^connected\nclosed$' 0 'sni=localhost'
    oracle no_sni_for_address ok p256 handshake '' $LOCAL '^connected\nclosed$' 0 'sni=none'
    oracle session_id ok p256 handshake '' $LOCAL '^connected\nclosed$' 0 'session_id_length=32'
    oracle ticket_ignored ticket p256 twice again $LOCAL '^line echo: again\nline echo: again\nclosed$' 0 'sent NewSessionTicket'
    oracle keyupdate keyupdate p256 twice again $LOCAL '^line echo: again\nline echo: again\nclosed$' 0 'client KeyUpdate'
    oracle chain_takes_leaf chain p256 echo hello $LOCAL '^line echo: hello\nclosed$' 0 -
    oracle coalesced_flight coalesce p256 echo hello $LOCAL '^line echo: hello\nclosed$' 0 -
    oracle split_messages split p256 echo hello $LOCAL '^line echo: hello\nclosed$' 0 -
    oracle one_octet_segments dribble p256 echo hello $LOCAL '^line echo: hello\nclosed$' 0 -
    oracle large_response big p256 read_all $'go\n' $LOCAL '^read_all 100000\nclosed$' 0 'sent 100000 octets'
    oracle close_notify_ends_read_all close_notify p256 read_all $'go\n' $LOCAL '^read_all 9\nclosed$' 0 -
    oracle http_get ok p256 get /index $LOCAL '^status 200\nbody 15\nhello over tls\n\nclosed$' 0 'line b.GET /index'

    # The ones that are wrong.
    PIN_OVERRIDE=$(printf '0%.0s' {1..64}) oracle wrong_pin ok p256 handshake '' $LOCAL '^error: the server.s key is not the pinned key$' 1 'client alert 42'
    oracle bad_finished bad_finished p256 handshake '' $LOCAL "^error: the server's Finished does not verify$" 1 'client alert 51'
    oracle bad_signature bad_signature p256 handshake '' $LOCAL "^error: the server's CertificateVerify signature is wrong$" 1 'client alert 51'
    oracle bad_signature_p384 bad_signature p384 handshake '' $LOCAL "^error: the server's CertificateVerify signature is wrong$" 1 'client alert 51'
    oracle scheme_not_offered wrong_scheme p256 handshake '' $LOCAL '^error: the server signed with a scheme that was not offered' 1 'client alert 47'
    oracle scheme_wrong_curve scheme_mismatch p256 handshake '' $LOCAL '^error: the server signed with a scheme that was not offered' 1 'client alert 47'
    oracle bad_record_tag bad_record p256 handshake '' $LOCAL "^error: a record's authentication tag did not verify$" 1 -
    oracle hello_retry_request hrr p256 handshake '' $LOCAL '^error: the server sent a HelloRetryRequest' 1 'client alert 40'
    oracle downgrade_sentinel downgrade p256 handshake '' $LOCAL '^error: the server.s random carries the TLS downgrade sentinel$' 1 'client alert 47'
    oracle tls12_server tls12 p256 handshake '' $LOCAL '^error: the server does not speak TLS 1.3$' 1 'client alert 70'
    oracle session_id_not_echoed bad_session_id p256 handshake '' $LOCAL "^error: the server's hello is not what was offered$" 1 'client alert 47'
    oracle zero_shared_secret zero_share p256 handshake '' $LOCAL "^error: the server's hello is not what was offered$" 1 'client alert 47'
    oracle unoffered_hello_extension server_hello_extra p256 handshake '' $LOCAL '^error: the server used an extension that was not offered$' 1 'client alert 110'
    oracle unoffered_ee_extension ee_extra p256 handshake '' $LOCAL '^error: the server used an extension that was not offered$' 1 'client alert 110'
    oracle certificate_request cert_request p256 handshake '' $LOCAL '^error: the server asked for a client certificate$' 1 'client alert 40'
    oracle no_certificate no_certificate p256 handshake '' $LOCAL "^error: the server's certificate is missing or unreadable$" 1 'client alert 42'
    oracle fatal_alert_in_handshake alert_after_hello p256 handshake '' $LOCAL '^error: the server sent a fatal alert$' 1 -
    oracle fatal_alert_after_handshake peer_alert p256 read_all '' $LOCAL '^error: the server sent a fatal alert$' 1 -
    oracle truncated_response truncate p256 read_all $'go\n' $LOCAL '^error: the connection ended without a close_notify$' 1 'closing without close_notify'
    oracle truncated_after_all_data truncate_clean p256 read_all $'go\n' $LOCAL '^error: the connection ended without a close_notify$' 1 -
fi

# --- openssl s_server ------------------------------------------------------------

if ! command -v openssl >/dev/null 2>&1; then
    skip "openssl s_server checks: openssl is not installed"
else
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
        -keyout "$WORK/key.pem" -out "$WORK/cert.pem" -subj /CN=localhost -days 1 >/dev/null 2>&1
    PIN=$(openssl x509 -in "$WORK/cert.pem" -pubkey -noout | openssl pkey -pubin -outform der 2>/dev/null \
        | openssl dgst -sha256 -binary | od -An -v -tx1 | tr -d ' \n')
    if [ -z "$PIN" ]; then
        skip "openssl s_server checks: cannot make a certificate"
    else
        # serve <name> <s_server flags...>: sets SERVER_PORT, or returns 1.
        serve() {
            local name=$1
            shift
            SERVER_PORT=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
            openssl s_server -accept "127.0.0.1:$SERVER_PORT" -cert "$WORK/cert.pem" -key "$WORK/key.pem" "$@" \
                >"$WORK/s_server.$name.log" 2>&1 </dev/null &
            SERVER_PID=$!
            pids+=("$SERVER_PID")
            local tries=0
            until python3 -c "import socket; socket.create_connection(('127.0.0.1', $SERVER_PORT), 1).close()" 2>/dev/null; do
                tries=$((tries + 1))
                if [ $tries -gt 100 ] || ! kill -0 "$SERVER_PID" 2>/dev/null; then
                    skip "openssl s_server $name: it did not start ($(tail -1 "$WORK/s_server.$name.log"))"
                    return 1
                fi
                python3 -c 'import time; time.sleep(0.05)'
            done
            # The probe above was a connection s_server saw and dropped.
            return 0
        }

        live() {
            local name=$1 want=$2 want_rc=$3
            shift 3
            local text rc
            text=$(timeout 60 "$CLIENT" "$@" 2>"$WORK/client.$name.err")
            rc=$?
            if [ "$rc" -ne "$want_rc" ]; then
                bad "$name" "client exited $rc, wanted $want_rc" "output: $text" "$(tail -3 "$WORK/client.$name.err")"
            elif ! matches "$want" <<<"$text"; then
                bad "$name" "output did not match /$want/" "got: $(printf '%s' "$text" | head -c 400)"
            else
                note "$name"
            fi
        }

        SUITES=TLS_CHACHA20_POLY1305_SHA256
        if serve www -tls1_3 -ciphersuites $SUITES -www; then
            live openssl_http_get '^status 200\nbody [0-9]+\n(.|\n)*closed$' 0 get 127.0.0.1 "$SERVER_PORT" "$PIN" /
            live openssl_wrong_pin '^error: the server.s key is not the pinned key$' 1 \
                handshake 127.0.0.1 "$SERVER_PORT" 0000000000000000000000000000000000000000000000000000000000000000
            kill "$SERVER_PID" 2>/dev/null
        fi
        if serve rev -tls1_3 -ciphersuites $SUITES -rev; then
            live openssl_echo '^line olleh\nclosed$' 0 echo 127.0.0.1 "$SERVER_PORT" "$PIN" hello
            live openssl_echo_twice '^line dlrow\nline dlrow\nclosed$' 0 twice 127.0.0.1 "$SERVER_PORT" "$PIN" world
            kill "$SERVER_PID" 2>/dev/null
        fi
        if serve tls12 -tls1_2 -www; then
            live openssl_tls12_only '^error: the server (does not speak TLS 1.3|sent a fatal alert)$' 1 handshake 127.0.0.1 "$SERVER_PORT" "$PIN"
            kill "$SERVER_PID" 2>/dev/null
        fi
        # A server that will only negotiate AES: no common suite, so a handshake_failure.
        if serve aes_only -tls1_3 -ciphersuites TLS_AES_128_GCM_SHA256 -www; then
            live openssl_no_common_suite '^error: ' 1 handshake 127.0.0.1 "$SERVER_PORT" "$PIN"
            kill "$SERVER_PID" 2>/dev/null
        fi
    fi
fi

# --- github.com ------------------------------------------------------------------

github_smoke() {
    command -v openssl >/dev/null 2>&1 || { skip "github.com: openssl is not installed"; return; }
    local chain=$WORK/github.pem
    if ! timeout 20 openssl s_client -connect github.com:443 -servername github.com -tls1_3 \
            -ciphersuites TLS_CHACHA20_POLY1305_SHA256 </dev/null >"$chain" 2>/dev/null; then
        skip "github.com: no network, or no ChaCha20-Poly1305 TLS 1.3 from it"
        return
    fi
    local key=$WORK/github.key
    openssl x509 -in "$chain" -pubkey -noout 2>/dev/null | openssl pkey -pubin -outform der >"$key" 2>/dev/null
    if [ ! -s "$key" ]; then
        skip "github.com: could not read its certificate"
        return
    fi
    if ! openssl pkey -pubin -inform der -in "$key" -text -noout 2>/dev/null | grep -q 'NIST CURVE: P-256\|NIST CURVE: P-384'; then
        skip "github.com: its leaf key is not ECDSA P-256 or P-384"
        return
    fi
    local pin text rc
    pin=$(openssl dgst -sha256 -binary <"$key" | od -An -v -tx1 | tr -d ' \n')
    text=$(timeout 60 "$CLIENT" get github.com 443 "$pin" / 2>"$WORK/github.err")
    rc=$?
    if [ $rc -eq 0 ] && grep -Eq '^status [0-9]+$' <<<"$text"; then
        note "github.com: $(printf '%s' "$text" | head -1), pinned by its SPKI hash"
    else
        bad "github.com" "client exited $rc" "$(printf '%s' "$text" | head -3 | cut -c1-200)" "$(tail -3 "$WORK/github.err")"
    fi
}
github_smoke

echo
if [ $fail -eq 0 ]; then
    printf '\033[32mall %d tls checks passed\033[0m\n' "$pass"
else
    printf '\033[31m%d of %d tls checks FAILED\033[0m\n' "$fail" "$((pass + fail))"
fi
exit $fail
