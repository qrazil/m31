#!/usr/bin/env bash
# Every check for TLS 1.3 session resumption in the client (`lib/tls.m31`,
# `lib/tlsresume.m31`, the ticket queue of `lib/tls13record.m31`), against
# peers that are not this repository. Modelled on `apps/tls/test_tls12.sh`.
#
#   bash apps/tls/test_resume.sh
#
#   resume_server.py    a TLS 1.3 server written from RFC 8446 that issues tickets and
#                       resumes them, recomputes every PSK binder itself, and misbehaves
#                       on request: every way the client must refuse is here with the
#                       error it must give and the alert it must send.
#   openssl s_server   the real thing: stateless tickets under each of the three TLS 1.3
#                       suites (SHA-256 and SHA-384 binders), no tickets, a server that
#                       restarted and no longer knows the ticket, and a certificate that
#                       expires before the ticket does.
#
# The client is `t_resume_client.m31`: a list of connections ("steps") over one
# session cache, each printing whether it was resumed and how many sessions the cache holds.
# Every peer is on loopback and disposable. The compiler is `LANGC` (default
# `./target/debug/m31c`, built with `cargo build`).
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
    for f in lib/tls.m31 lib/tlsresume.m31 lib/tls13record.m31 lib/tls13schedule.m31 apps/tls/t_resume_client.m31; do
        "$LANGC" fmt --check "$f" || echo "$f"
    done 2>&1
) && [ -z "$out" ]; then
    note "source is formatted"
else
    bad "source is not formatted (run: m31c fmt <file>)" "$out"
fi

if ! python3 -c 'import cryptography' 2>/dev/null; then
    bad "oracles" "the 'cryptography' package is not importable -- pip install cryptography"
    exit 1
fi
build t_resume_client || { printf '\033[31m%d of %d resume checks FAILED\033[0m\n' "$fail" "$((pass + fail))"; exit 1; }
CLIENT=$WORK/t_resume_client

REFUSED_HELLO="the server's hello is not what was offered"
REFUSED_EXTENSION='the server used an extension that was not offered'
REFUSED_FINISHED="the server's Finished does not verify"
REFUSED_ORDER='a handshake message arrived where it is not allowed'
REFUSED_MALFORMED='a handshake message is malformed'
REFUSED_TAG="a record's authentication tag did not verify"
REFUSED_PIN="the server.s key is not the pinned key"

# scenario <name> <modes> <capacity> <output regex> <oracle log regex or -> <log regex that must NOT match or -> <step>...
# The step output is matched as a whole; the oracle's log line by line.
scenario() {
    local name=$1 modes=$2 capacity=$3 want=$4 want_log=$5 deny_log=$6
    shift 6
    local log=$WORK/oracle.$name.log out=$WORK/client.$name.out
    python3 apps/tls/resume_server.py "$modes" >"$log" 2>"$WORK/oracle.$name.err" &
    local pid=$!
    pids+=("$pid")
    if ! await_line "$log" '^READY ' "$pid"; then
        bad "$name" "the oracle did not start" "$(tail -5 "$WORK/oracle.$name.err")"
        return
    fi
    local port pin
    read -r _ port pin <"$log"
    timeout 90 "$CLIENT" "$port" "$pin" "$capacity" "$@" >"$out" 2>"$WORK/client.$name.err"
    local rc=$?
    wait "$pid" 2>/dev/null
    local text
    text=$(cat "$out")
    if [ "$rc" -ne 0 ]; then
        bad "$name" "client exited $rc" "output: $text" "$(tail -3 "$WORK/client.$name.err")"
    elif ! matches "$want" <<<"$text"; then
        bad "$name" "output did not match /$want/" "got: $text" "oracle: $(tail -6 "$log")"
    elif [ "$want_log" != - ] && ! grep -Eq "$want_log" "$log"; then
        bad "$name" "the oracle's log did not match /$want_log/" "$(tail -12 "$log")"
    elif [ "$deny_log" != - ] && grep -Eq "$deny_log" "$log"; then
        bad "$name" "the oracle's log matched /$deny_log/" "$(grep -E "$deny_log" "$log" | head -3)"
    elif grep -q 'server error' "$log" && [ "${ALLOW_SERVER_ERROR:-0}" != 1 ]; then
        bad "$name" "the oracle itself failed" "$(grep 'server error' "$log")"
    else
        note "$name"
    fi
}

NEVER='binder=BAD|ticket_reused|early_data=present|psk_last=NO|offered extensions=([0-9]+,)*42(,|$)'

# --- the happy paths ---------------------------------------------------------------

scenario resume_basic full,resume,resume 8 \
    '^step 1 a.example resumed=false cache=1\nstep 2 a.example resumed=true cache=1\nstep 3 a.example resumed=true cache=1$' \
    '2 psk binder=ok' "$NEVER" \
    a.example:same:1000:get a.example:same:1100:get a.example:same:1200:get
scenario resume_second_has_psk_and_modes full,resume 8 '^step 1 .* resumed=false.*\nstep 2 .* resumed=true' \
    '2 offered extensions=0,5,10,11,13,50,23,65281,43,51,45,41$' - \
    a.example:same:1000:get a.example:same:1100:get
scenario resume_skips_certificate_only_on_a_resumption full,resume 8 '^step 1 a.example resumed=false.*\nstep 2 a.example resumed=true' \
    '2 resumption=accepted' - \
    a.example:same:1000:get a.example:same:1100:get
scenario resume_binder_and_schedule_verified_by_server full,resume 8 '^step 1 .*\nstep 2 a.example resumed=true' \
    '2 psk binder=ok' 'binder=BAD' \
    a.example:same:1000:get a.example:same:1100:get
scenario resume_client_finished_under_psk full,resume 8 '^step 1 .*\nstep 2 a.example resumed=true' \
    '2 client_finished ok' 'client_finished BAD' \
    a.example:same:1000:get a.example:same:1100:get
scenario resume_age_is_reported full,resume 8 '^step 1 .*\nstep 2 a.example resumed=true' \
    '2 psk age_ms=100000$' - \
    a.example:same:1000:get a.example:same:1100:get
scenario resume_no_early_data_ever full,resume,resume 8 '^step 1 .*\nstep 2 a.example resumed=true.*\nstep 3 a.example resumed=true' \
    '3 early_data=absent' 'early_data=present|offered extensions=([0-9]+,)*42(,|$)' \
    a.example:same:1000:get a.example:same:1100:get a.example:same:1200:get
scenario resume_psk_dhe_only full,resume 8 '^step 1 .*\nstep 2 a.example resumed=true' \
    '2 psk_modes=0101$' - \
    a.example:same:1000:get a.example:same:1100:get
scenario resume_ticket_is_used_once full,resume,resume,resume 8 \
    '^step 1 .* cache=1\nstep 2 .* resumed=true cache=1\nstep 3 .* resumed=true cache=1\nstep 4 .* resumed=true cache=1$' \
    '4 psk binder=ok' "$NEVER" \
    a.example:same:1000:get a.example:same:1100:get a.example:same:1200:get a.example:same:1300:get
scenario resume_ticket_with_early_data_extension_is_kept ticket_early_data,resume 8 \
    '^step 1 a.example resumed=false cache=1\nstep 2 a.example resumed=true' \
    '2 psk binder=ok' 'early_data=present|offered extensions=([0-9]+,)*42(,|$)' \
    a.example:same:1000:get a.example:same:1100:get
scenario resume_many_tickets_newest_is_offered ticket_many,resume 32 \
    '^step 1 a.example resumed=false cache=16\nstep 2 a.example resumed=true cache=16$' \
    '2 psk binder=ok' "$NEVER" \
    a.example:same:1000:get a.example:same:1100:get
scenario resume_cache_is_bounded ticket_many,resume 8 \
    '^step 1 a.example resumed=false cache=8\nstep 2 a.example resumed=true cache=8$' \
    '2 psk binder=ok' "$NEVER" \
    a.example:same:1000:get a.example:same:1100:get

# --- expiry ------------------------------------------------------------------------

scenario resume_just_before_expiry full,resume 8 '^step 1 .*\nstep 2 a.example resumed=true' \
    '2 psk age_ms=3599000$' - \
    a.example:same:1000:get a.example:same:4599:get
scenario resume_expired_ticket_is_not_offered full,resume 8 '^step 1 .*\nstep 2 a.example resumed=false cache=1$' \
    '2 resumption=no' '2 psk_offered' \
    a.example:same:1000:get a.example:same:4600:get
scenario resume_short_ticket_before full,resume 8 '^step 1 .*\nstep 2 a.example resumed=true' \
    '2 psk binder=ok' - \
    a.example:same:1000:get a.example:same:1099:get
scenario resume_short_ticket_after ticket_short,resume 8 '^step 1 .*\nstep 2 a.example resumed=false' \
    - '2 psk_offered' \
    a.example:same:1000:get a.example:same:1100:get
scenario resume_lifetime_is_capped_at_seven_days_before ticket_long,resume 8 '^step 1 .*\nstep 2 a.example resumed=true' \
    '2 psk binder=ok' - \
    a.example:same:1000:get a.example:same:605799:get
scenario resume_lifetime_is_capped_at_seven_days_after ticket_long,resume 8 '^step 1 .*\nstep 2 a.example resumed=false' \
    - '2 psk_offered' \
    a.example:same:1000:get a.example:same:605800:get
scenario resume_zero_lifetime_is_not_stored ticket_zero,ticket_zero 8 '^step 1 a.example resumed=false cache=0\nstep 2 a.example resumed=false cache=0$' \
    - 'psk_offered' \
    a.example:same:1000:get a.example:same:1100:get
scenario resume_clock_going_backwards_is_not_a_negative_age full,resume 8 '^step 1 .*\nstep 2 a.example resumed=true' \
    '2 psk age_ms=0$' - \
    a.example:same:5000:get a.example:same:4000:get

# --- what a session is bound to --------------------------------------------------------

scenario resume_wrong_hostname_gets_nothing full,full,resume 8 \
    '^step 1 a.example resumed=false cache=1\nstep 2 b.example resumed=false cache=2\nstep 3 a.example resumed=true cache=2$' \
    '2 sni=b.example' '2 psk_offered|2 resumption=accepted' \
    a.example:same:1000:get b.example:same:1001:get a.example:same:1002:get
scenario resume_wrong_hostname_second_ticket_is_b_s full,full,resume,resume 8 \
    '^step 1 .*\nstep 2 .*\nstep 3 b.example resumed=true.*\nstep 4 a.example resumed=true' \
    '4 sni=a.example' "$NEVER" \
    a.example:same:1000:get b.example:same:1001:get b.example:same:1002:get a.example:same:1003:get
scenario resume_other_trust_is_not_resumed_and_still_validated full,full,resume 8 \
    "^step 1 a.example resumed=false cache=1\nstep 2 a.example error=$REFUSED_PIN cache=1\nstep 3 a.example resumed=true cache=1\$" \
    - '2 psk_offered|2 resumption=accepted' \
    a.example:same:1000:get a.example:other:1001:get a.example:same:1002:get
scenario resume_cache_of_one_keeps_only_the_newest full,full,full 1 \
    '^step 1 a.example resumed=false cache=1\nstep 2 b.example resumed=false cache=1\nstep 3 a.example resumed=false cache=1$' \
    '3 sni=a.example' '3 psk_offered' \
    a.example:same:1000:get b.example:same:1001:get a.example:same:1002:get
scenario resume_cleared_cache_forgets full,full 8 \
    '^step 1 a.example resumed=false cache=1\nstep 2 clear cache=0\nstep 3 a.example resumed=false cache=1$' \
    '2 sni=a.example' '2 psk_offered' \
    a.example:same:1000:get a.example:same:1001:clear a.example:same:1002:get
scenario resume_cache_of_zero_asks_for_no_tickets full,full 0 \
    '^step 1 a.example resumed=false cache=0\nstep 2 a.example resumed=false cache=0$' \
    '1 psk_modes=none' 'psk_offered|1 sent 1 ticket' \
    a.example:same:1000:get a.example:same:1001:get
scenario resume_server_that_sends_none full_noticket,full_noticket 8 \
    '^step 1 a.example resumed=false cache=0\nstep 2 a.example resumed=false cache=0$' \
    - 'psk_offered' \
    a.example:same:1000:get a.example:same:1001:get
scenario resume_server_that_forgot_falls_back_to_a_full_handshake full,ignore 8 \
    '^step 1 a.example resumed=false cache=1\nstep 2 a.example resumed=false cache=1$' \
    '2 psk_offered identities=1' '2 resumption=accepted' \
    a.example:same:1000:get a.example:same:1100:get
scenario resume_unknown_ticket_falls_back full,full 8 \
    '^step 1 a.example resumed=false cache=1\nstep 2 a.example resumed=false cache=1$' \
    - - \
    a.example:same:1000:get a.example:same:1100:get

# --- servers that must be refused ------------------------------------------------------------

refusal() { # refusal <name> <mode> <message> <alert> [allow server error]
    ALLOW_SERVER_ERROR=${5:-0} scenario "$1" "full,$2" 8 "^step 1 .*\nstep 2 a.example error=$3 cache=0\$" \
        "$([ "$4" = - ] && echo - || echo "client alert $4")" - \
        a.example:same:1000:get a.example:same:1100:get
}
refusal resume_refuses_an_unoffered_identity bad_identity "$REFUSED_HELLO" 47
refusal resume_refuses_a_psk_under_another_suite psk_wrong_suite "$REFUSED_HELLO" 47
refusal resume_refuses_psk_without_key_share psk_no_dhe "$REFUSED_HELLO" 47
refusal resume_refuses_a_bad_finished_under_psk psk_bad_finished "$REFUSED_FINISHED" 51
refusal resume_refuses_a_certificate_in_a_psk_handshake psk_certificate "$REFUSED_ORDER" 10
refusal resume_refuses_a_server_with_another_psk wrong_psk "$REFUSED_TAG" - 1
scenario resume_refuses_an_unoffered_psk_extension unsolicited 8 "^step 1 a.example error=$REFUSED_EXTENSION cache=0\$" \
    'client alert 110' - a.example:same:1000:get

for mode in ticket_malformed ticket_trailing ticket_empty ticket_bad_ext; do
    scenario "resume_refuses_hostile_ticket_$mode" "$mode,resume" 8 \
        "^step 1 a.example error=$REFUSED_MALFORMED cache=0\nstep 2 a.example resumed=false cache=1\$" \
        'client alert 50' '2 psk_offered' \
        a.example:same:1000:get a.example:same:1100:get
done

# --- openssl s_server ------------------------------------------------------------

if ! command -v openssl >/dev/null 2>&1; then
    skip "openssl s_server checks: openssl is not installed"
else
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout "$WORK/ec.key" -out "$WORK/ec.pem" \
        -subj /CN=localhost -days 1 >/dev/null 2>&1
    EC_PIN=$(openssl x509 -in "$WORK/ec.pem" -pubkey -noout | openssl pkey -pubin -outform der 2>/dev/null \
        | openssl dgst -sha256 -binary | od -An -v -tx1 | tr -d ' \n')
    if [ -z "$EC_PIN" ]; then
        skip "openssl s_server checks: cannot make a certificate"
    else
        serve() { # serve <name> <cert> <key> <s_server flags...>: sets SERVER_PORT, or returns 1.
            local name=$1 cert=$2 key=$3
            shift 3
            SERVER_PORT=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
            openssl s_server -accept "127.0.0.1:$SERVER_PORT" -cert "$cert" -key "$key" "$@" \
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
            return 0
        }
        # live <name> <want> <pin or ca:file> <capacity> <step>... (steps without a port go to $SERVER_PORT)
        live() {
            local name=$1 want=$2 pin=$3 capacity=$4
            shift 4
            local text rc
            text=$(timeout 90 "$CLIENT" "$SERVER_PORT" "$pin" "$capacity" "$@" 2>"$WORK/client.$name.err")
            rc=$?
            if [ "$rc" -ne 0 ]; then
                bad "$name" "client exited $rc" "output: $text" "$(tail -3 "$WORK/client.$name.err")"
            elif ! matches "$want" <<<"$text"; then
                bad "$name" "output did not match /$want/" "got: $text"
            else
                note "$name"
            fi
        }
        THREE='^step 1 localhost resumed=false cache=[0-9]+\nstep 2 localhost resumed=true cache=[0-9]+\nstep 3 localhost resumed=true cache=[0-9]+$'
        STEPS="localhost:same:1000:get localhost:same:1100:get localhost:same:1200:get"
        for suite in TLS_CHACHA20_POLY1305_SHA256 TLS_AES_128_GCM_SHA256 TLS_AES_256_GCM_SHA384; do
            if serve "resume_$suite" "$WORK/ec.pem" "$WORK/ec.key" -www -tls1_3 -ciphersuites "$suite" -num_tickets 2; then
                live "openssl_resume_$suite" "$THREE" "$EC_PIN" 8 $STEPS
            fi
        done
        if serve resume_one_ticket "$WORK/ec.pem" "$WORK/ec.key" -www -tls1_3 -num_tickets 1; then
            live openssl_resume_one_ticket '^step 1 localhost resumed=false cache=1\nstep 2 localhost resumed=true cache=1\nstep 3 localhost resumed=true cache=1$' "$EC_PIN" 8 $STEPS
        fi
        if serve no_tickets "$WORK/ec.pem" "$WORK/ec.key" -www -tls1_3 -num_tickets 0; then
            live openssl_no_tickets '^step 1 localhost resumed=false cache=0\nstep 2 localhost resumed=false cache=0\nstep 3 localhost resumed=false cache=0$' "$EC_PIN" 8 $STEPS
        fi
        if serve cache_off "$WORK/ec.pem" "$WORK/ec.key" -www -tls1_3; then
            live openssl_cache_off '^step 1 localhost resumed=false cache=0\nstep 2 localhost resumed=false cache=0$' "$EC_PIN" 0 \
                localhost:same:1000:get localhost:same:1100:get
        fi
        if serve host_a "$WORK/ec.pem" "$WORK/ec.key" -www -tls1_3 -num_tickets 1; then
            live openssl_wrong_hostname '^step 1 a.example resumed=false cache=1\nstep 2 b.example resumed=false cache=2\nstep 3 a.example resumed=true cache=2$' "$EC_PIN" 8 \
                a.example:same:1000:get b.example:same:1001:get a.example:same:1002:get
        fi
        if serve other_pin "$WORK/ec.pem" "$WORK/ec.key" -www -tls1_3 -num_tickets 1; then
            live openssl_other_trust "^step 1 localhost resumed=false cache=1\nstep 2 localhost error=$REFUSED_PIN cache=1\nstep 3 localhost resumed=true cache=1\$" "$EC_PIN" 8 \
                localhost:same:1000:get localhost:other:1001:get localhost:same:1002:get
        fi
        if serve restart_a "$WORK/ec.pem" "$WORK/ec.key" -www -tls1_3 -num_tickets 1; then
            PORT_A=$SERVER_PORT
            if serve restart_b "$WORK/ec.pem" "$WORK/ec.key" -www -tls1_3 -num_tickets 1; then
                live openssl_server_forgot_the_ticket '^step 1 localhost resumed=false cache=1\nstep 2 localhost resumed=false cache=1\nstep 3 localhost resumed=true cache=1$' "$EC_PIN" 8 \
                    "localhost:same:1000:get:$PORT_A" "localhost:same:1100:get:$SERVER_PORT" "localhost:same:1200:get:$SERVER_PORT"
            fi
        fi

        # A certificate that expires before its tickets do. The CA, and a leaf valid
        # from 1700000000 to 1700003000 (epoch seconds); the clock is the step's `now`.
        python3 - "$WORK" <<'PYEOF'
import datetime, sys
from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.x509.oid import NameOID
work = sys.argv[1]
def when(epoch):
    return datetime.datetime.fromtimestamp(epoch, datetime.timezone.utc)
ca_key = ec.generate_private_key(ec.SECP256R1())
ca_name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "resume test ca")])
ca = (x509.CertificateBuilder().subject_name(ca_name).issuer_name(ca_name).public_key(ca_key.public_key())
      .serial_number(1).not_valid_before(when(1600000000)).not_valid_after(when(1900000000))
      .add_extension(x509.BasicConstraints(ca=True, path_length=None), critical=True)
      .add_extension(x509.KeyUsage(False, False, False, False, False, True, True, False, False), critical=True)
      .sign(ca_key, hashes.SHA256()))
leaf_key = ec.generate_private_key(ec.SECP256R1())
leaf = (x509.CertificateBuilder().subject_name(x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "localhost")]))
        .issuer_name(ca_name).public_key(leaf_key.public_key()).serial_number(2)
        .not_valid_before(when(1700000000)).not_valid_after(when(1700003000))
        .add_extension(x509.SubjectAlternativeName([x509.DNSName("localhost")]), critical=False)
        .add_extension(x509.BasicConstraints(ca=False, path_length=None), critical=True)
        .add_extension(x509.ExtendedKeyUsage([x509.oid.ExtendedKeyUsageOID.SERVER_AUTH]), critical=False)
        .sign(ca_key, hashes.SHA256()))
open(work + "/ca.pem", "wb").write(ca.public_bytes(serialization.Encoding.PEM))
open(work + "/leaf.pem", "wb").write(leaf.public_bytes(serialization.Encoding.PEM))
open(work + "/leaf.key", "wb").write(leaf_key.private_bytes(
    serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8, serialization.NoEncryption()))
PYEOF
        if [ -s "$WORK/leaf.key" ] && serve expiry "$WORK/leaf.pem" "$WORK/leaf.key" -www -tls1_3 -num_tickets 1; then
            live openssl_session_does_not_outlive_the_certificate \
                '^step 1 localhost resumed=false cache=1\nstep 2 localhost resumed=true cache=1\nstep 3 localhost error=a certificate in the server.s chain has expired cache=0$' "ca:$WORK/ca.pem" 8 \
                localhost:same:1700000100:get localhost:same:1700002900:get localhost:same:1700003100:get
        fi
        if [ -s "$WORK/leaf.key" ] && serve expiry_chain "$WORK/leaf.pem" "$WORK/leaf.key" -www -tls1_3 -num_tickets 1; then
            live openssl_a_chain_of_resumptions_does_not_extend_trust \
                '^step 1 localhost resumed=false cache=1\nstep 2 localhost resumed=true cache=1\nstep 3 localhost resumed=true cache=1\nstep 4 localhost resumed=true cache=1$' "ca:$WORK/ca.pem" 8 \
                localhost:same:1700000100:get localhost:same:1700001000:get localhost:same:1700001900:get localhost:same:1700002800:get
        fi
    fi
fi

echo
if [ $fail -eq 0 ]; then
    printf '\033[32mall %d resume checks passed\033[0m\n' "$pass"
else
    printf '\033[31m%d of %d resume checks FAILED\033[0m\n' "$fail" "$((pass + fail))"
fi
exit $fail
