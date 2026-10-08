#!/usr/bin/env bash
# Every check for OCSP stapling in the client (`lib/ocsp.m31` and the revocation code
# of `lib/tls.m31`), against peers and fixtures that are not this repository. Modelled
# on `apps/tls/test_clientcert.sh`.
#
#   bash apps/tls/test_ocsp.sh
#
#   ocsp_fixtures.py     builds a CA, leaves, delegated responders (right and wrong)
#                        and ~65 OCSP responses -- good ones and every kind of bad one
#                        -- with a DER writer of its own and the `cryptography` package,
#                        and writes down what each must give (`cases.txt`). The
#                        expected verdicts are the oracle: they come from RFC 6960, not
#                        from the code under test.
#   openssl ocsp         makes responses the way real responders do (`-index`, signed by
#                        the issuer or by a delegated responder, SHA-1 and SHA-256
#                        CertIDs, `-resp_no_certs`), and verifies the fixtures' good and
#                        bad responses independently of this repository.
#   openssl s_server     `-status_file`: staples a response in TLS 1.3 and TLS 1.2, with
#                        a good, revoked, expired, wrong-certificate or must-staple case,
#                        and resumption after a staple.
#   ocsp_server.py       a TLS 1.3 server that staples wrongly in every way the
#                        extension allows (bad framing, duplicates, wrong place, ...).
#   tls12_server.py      the same for TLS 1.2 (`OCSP_CHAIN`, `OCSP_KEY`, `OCSP_RESPONSE`,
#                        the `staple_*` modes): CertificateStatus sent or not, twice,
#                        late, unpromised, malformed.
#
# The clients are `t_ocsp_check.m31` (one response, one certificate, one clock) and
# `t_ocsp_client.m31` (N connections over one session cache). Every peer is on
# loopback and disposable. The compiler is `LANGC` (default `./target/debug/m31c`).
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
    for f in lib/ocsp.m31 lib/sha1digest.m31 lib/tls.m31 lib/x509.m31 lib/x509_chain.m31 lib/http.m31 \
             apps/tls/t_ocsp_check.m31 apps/tls/t_ocsp_client.m31 apps/tls/t_chain_client.m31; do
        "$LANGC" fmt --check "$f" || echo "$f"
    done 2>&1
) && [ -z "$out" ]; then
    note "ocsp_source_is_formatted"
else
    bad "ocsp_source_is_not_formatted (run: m31c fmt <file>)" "$out"
fi

if ! python3 -c 'import cryptography' 2>/dev/null; then
    bad "oracles" "the 'cryptography' package is not importable -- pip install cryptography"
    exit 1
fi
build t_ocsp_check || { printf '\033[31m%d of %d ocsp checks FAILED\033[0m\n' "$fail" "$((pass + fail))"; exit 1; }
build t_ocsp_client || { printf '\033[31m%d of %d ocsp checks FAILED\033[0m\n' "$fail" "$((pass + fail))"; exit 1; }
CHECK=$WORK/t_ocsp_check
CLIENT=$WORK/t_ocsp_client

# --- fixtures ------------------------------------------------------------------

F=$WORK/fx
mkdir -p "$F"
python3 apps/tls/ocsp_fixtures.py "$F" 2>"$WORK/fixtures.err"
if [ ! -s "$F/cases.txt" ]; then
    bad "fixtures" "python could not make the test responses" "$(tail -3 "$WORK/fixtures.err")"
    exit 1
fi
cat "$F/leaf.pem" "$F/ca.pem" >"$F/leaf_chain.pem"
cat "$F/mustStaple.pem" "$F/ca.pem" >"$F/muststaple_chain.pem"
NOW=$(date +%s)

# --- the response checker: every fixture, against the verdict RFC 6960 gives ------

# check <name> <response> <leaf> <issuer> <seconds from now> <want: ok | Error variant>
check() {
    local name=$1 response=$2 leaf=$3 issuer=$4 offset=$5 want=$6 text got
    text=$(timeout 30 "$CHECK" "$response" "$leaf" "$issuer" "$((NOW + offset))" 2>"$WORK/check.err")
    case "$text" in
        ok\ this_update=*) got=ok ;;
        error\ *) got=${text#error } ;;
        *) got="(no verdict: $text $(tail -1 "$WORK/check.err"))" ;;
    esac
    if [ "$got" = "$want" ]; then
        note "ocsp_$name"
    else
        bad "ocsp_$name" "wanted $want, got $got"
    fi
}
while IFS='|' read -r name leaf issuer offset want; do
    check "$name" "$F/c_$name.der" "$F/$leaf" "$F/$issuer" "$offset" "$want"
done <"$F/cases.txt"

# The times it reports are the response's.
text=$("$CHECK" "$F/c_good_sha1_certid.der" "$F/leaf.pem" "$F/ca.pem" "$NOW")
if matches '^ok this_update=\d+ next_update=\d+$' <<<"$text" \
   && [ "$(awk '{split($2,a,"="); split($3,b,"="); print b[2]-a[2]}' <<<"$text")" = 608400 ]; then
    note "ocsp_reports_this_update_and_next_update"
else
    bad "ocsp_reports_this_update_and_next_update" "$text"
fi

# --- the same fixtures, judged by openssl ----------------------------------------------------

if ! command -v openssl >/dev/null 2>&1; then
    skip "openssl ocsp checks: openssl is not installed"
else
    # openssl_says <name> <response> <leaf> <regex over its output> [-sha256]
    openssl_says() {
        local name=$1 response=$2 leaf=$3 want=$4 text
        shift 4
        text=$(openssl ocsp -respin "$response" -issuer "$F/ca.pem" "$@" -cert "$F/$leaf" -CAfile "$F/ca.pem" 2>&1)
        if matches "$want" <<<"$text"; then
            note "$name"
        else
            bad "$name" "openssl said: $text"
        fi
    }
    for name in good_sha1_certid good_by_key_hash good_ecdsa_sha384 good_delegated good_delegated_by_key_hash good_delegated_rsa; do
        openssl_says "ocsp_openssl_agrees_$name" "$F/c_$name.der" leaf.pem 'Response verify OK\n.*: good\n'
    done
    openssl_says ocsp_openssl_agrees_good_sha256_certid "$F/c_good_sha256_certid.der" leaf.pem 'Response verify OK\n.*: good\n' -sha256
    openssl_says ocsp_openssl_agrees_revoked "$F/c_revoked.der" leaf.pem 'Response verify OK\n.*: revoked\n'
    for name in bad_signature signed_by_other_key delegated_responder_no_eku delegated_responder_rogue_issuer delegated_responder_expired delegated_not_carried; do
        openssl_says "ocsp_openssl_refuses_${name}" "$F/c_$name.der" leaf.pem 'Response Verify Failure'
    done

    # Responses made the way a responder makes them.
    request() { openssl ocsp -issuer "$F/ca.pem" -cert "$F/$1" -reqout "$WORK/request.der" >/dev/null 2>&1; }
    # respond <out> <leaf> <signer cert> <signer key> [openssl ocsp flags]
    respond() {
        local out=$1 leaf=$2 signer=$3 key=$4
        shift 4
        request "$leaf" && openssl ocsp -index "$F/index.txt" -CA "$F/ca.pem" -rsigner "$signer" -rkey "$key" \
            -reqin "$WORK/request.der" -respout "$out" "$@" >/dev/null 2>"$WORK/respond.err"
    }
    respond "$F/o_good.der" leaf.pem "$F/ca.pem" "$F/ca.key" -ndays 7
    respond "$F/o_revoked.der" revoked.pem "$F/ca.pem" "$F/ca.key" -ndays 7
    respond "$F/o_unknown.der" stranger.pem "$F/ca.pem" "$F/ca.key" -ndays 7
    respond "$F/o_delegated.der" leaf.pem "$F/responder.pem" "$F/responder.key" -ndays 7
    respond "$F/o_nocerts.der" leaf.pem "$F/ca.pem" "$F/ca.key" -ndays 7 -resp_no_certs
    respond "$F/o_sha1.der" leaf.pem "$F/ca.pem" "$F/ca.key" -ndays 7 -rmd sha1
    respond "$F/o_nonext.der" leaf.pem "$F/ca.pem" "$F/ca.key"
    respond "$F/o_hour.der" leaf.pem "$F/ca.pem" "$F/ca.key" -nmin 60
    respond "$F/o_muststaple.der" mustStaple.pem "$F/ca.pem" "$F/ca.key" -ndays 7
    if [ ! -s "$F/o_muststaple.der" ]; then
        bad "openssl_ocsp_responses" "openssl ocsp could not make a response" "$(tail -3 "$WORK/respond.err")"
    fi
    check openssl_ocsp_good "$F/o_good.der" "$F/leaf.pem" "$F/ca.pem" 0 ok
    check openssl_ocsp_revoked "$F/o_revoked.der" "$F/revoked.pem" "$F/ca.pem" 0 Revoked
    check openssl_ocsp_unknown_serial "$F/o_unknown.der" "$F/stranger.pem" "$F/ca.pem" 0 UnknownCertificate
    check openssl_ocsp_delegated_responder "$F/o_delegated.der" "$F/leaf.pem" "$F/ca.pem" 0 ok
    check openssl_ocsp_without_certificates "$F/o_nocerts.der" "$F/leaf.pem" "$F/ca.pem" 0 ok
    check openssl_ocsp_sha1_signature "$F/o_sha1.der" "$F/leaf.pem" "$F/ca.pem" 0 UnsupportedAlgorithm
    check openssl_ocsp_without_next_update "$F/o_nonext.der" "$F/leaf.pem" "$F/ca.pem" 0 Stale
    check openssl_ocsp_expired "$F/o_good.der" "$F/leaf.pem" "$F/ca.pem" $((8 * 86400)) Stale
    check openssl_ocsp_for_another_certificate "$F/o_good.der" "$F/leaf2.pem" "$F/ca.pem" 0 WrongCertificate
    check openssl_ocsp_for_another_issuer "$F/o_good.der" "$F/leaf.pem" "$F/other_ca.pem" 0 UnauthorizedResponder
    check openssl_ocsp_not_yet_valid "$F/o_good.der" "$F/leaf.pem" "$F/ca.pem" -3600 NotYetValid
fi

# --- hostile input: the checker must say Malformed (or anything but ok), never crash ---------

hostile() { # hostile <name> <file> <want regex over the verdict line>
    local name=$1 file=$2 want=$3 text rc
    text=$(timeout 30 "$CHECK" "$file" "$F/leaf.pem" "$F/ca.pem" "$NOW" 2>"$WORK/check.err")
    rc=$?
    if [ $rc -eq 0 ] && matches "$want" <<<"$text"; then
        note "ocsp_$name"
    else
        bad "ocsp_$name" "exit $rc, output: $text" "$(tail -2 "$WORK/check.err")"
    fi
}
: >"$WORK/empty.der"
hostile empty_input "$WORK/empty.der" '^error Malformed$'
head -c 1 /dev/urandom >"$WORK/one.der"
hostile one_octet "$WORK/one.der" '^error Malformed$'
head -c 4000 /dev/urandom >"$WORK/noise.der"
hostile random_noise "$WORK/noise.der" '^error Malformed$'
python3 -c 'import sys; sys.stdout.buffer.write(b"\x30\x84\xff\xff\xff\xff" + b"A" * 100)' >"$WORK/hugelen.der"
hostile enormous_length "$WORK/hugelen.der" '^error Malformed$'
python3 -c 'import sys; sys.stdout.buffer.write(b"\x30\x80" + b"\x30\x80" * 5000)' >"$WORK/indef.der"
hostile indefinite_length_nesting "$WORK/indef.der" '^error Malformed$'
python3 -c 'import sys; sys.stdout.buffer.write(b"\x30\x82\x27\x10" + b"\x30\x82\x26\xfa" * 1 + b"\x30" * 9980)' >"$WORK/deep.der"
hostile deep_nesting "$WORK/deep.der" '^error Malformed$'
cat "$F/c_good_sha1_certid.der" <(printf '\x00') >"$WORK/trailing.der"
hostile trailing_octet "$WORK/trailing.der" '^error Malformed$'
cat "$F/c_good_sha1_certid.der" "$F/c_good_sha1_certid.der" >"$WORK/doubled.der"
hostile two_responses_in_a_row "$WORK/doubled.der" '^error Malformed$'
hostile a_certificate_is_not_a_response "$F/leaf.pem" '^error Malformed$'

# Every proper prefix is Malformed; no single changed octet is ever accepted.
fuzz() { # fuzz <name> <response> <leaf>
    local name=$1 response=$2 leaf=$3 report
    report=$(python3 - "$CHECK" "$response" "$F/$leaf" "$F/ca.pem" "$NOW" <<'PYEOF'
import subprocess, sys
check, response, leaf, issuer, now = sys.argv[1:6]
good = open(response, "rb").read()
def run(data):
    open(response + ".try", "wb").write(data)
    done = subprocess.run([check, response + ".try", leaf, issuer, now], capture_output=True, timeout=30)
    return done.returncode, done.stdout.decode().strip()
problems = []
for length in range(len(good)):
    code, line = run(good[:length])
    if code != 0 or line != "error Malformed":
        problems.append("prefix %d: exit %d %r" % (length, code, line))
for index in range(len(good)):
    flipped = bytearray(good)
    flipped[index] ^= 0x40
    code, line = run(bytes(flipped))
    if code != 0 or not line.startswith("error "):
        problems.append("flip at %d: exit %d %r" % (index, code, line))
print("prefixes and flips: %d octets" % len(good))
for problem in problems[:8]:
    print("PROBLEM " + problem)
PYEOF
)
    if grep -q '^PROBLEM' <<<"$report"; then
        bad "$name" "$(grep '^PROBLEM' <<<"$report")"
    else
        note "$name"
    fi
}
fuzz ocsp_every_truncation_and_every_flipped_octet_of_an_issuer_signed_response "$F/c_good_sha1_certid.der" leaf.pem
fuzz ocsp_every_truncation_and_every_flipped_octet_of_a_delegated_response "$F/c_good_delegated.der" leaf.pem

# --- one server, one chain, many clients -------------------------------------------------------

REFUSED_REVOKED="the server's certificate is revoked"
REFUSED_STAPLE="the server's stapled OCSP response cannot be trusted"
REFUSED_REQUIRED='a stapled OCSP response was required and the server sent none'
REFUSED_MALFORMED='a handshake message is malformed'
REFUSED_ORDER='a handshake message arrived where it is not allowed'
REFUSED_HELLO="the server's hello is not what was offered"
REFUSED_EXTENSION='the server used an extension that was not offered'
OK='^conn 1 ok resumed=false stapled=true status=HTTP/1\.[01] 200 [Oo][Kk]$'
OK_UNSTAPLED='^conn 1 ok resumed=false stapled=false status=HTTP/1\.[01] 200 [Oo][Kk]$'
fails() { echo "^conn 1 error=$1 alert=-1\$"; }

# oracle13 <name> <chain> <key> <modes> <client mode> <offsets> <connections> <want> <oracle log regex or ->
oracle13() {
    local name=$1 chain=$2 key=$3 modes=$4 client_mode=$5 offsets=$6 connections=$7 want=$8 want_log=$9
    local log=$WORK/oracle.$name.log out=$WORK/client.$name.out
    python3 apps/tls/ocsp_server.py "$chain" "$key" "$modes" >"$log" 2>"$WORK/oracle.$name.err" &
    local pid=$!
    pids+=("$pid")
    if ! await_line "$log" '^READY ' "$pid"; then
        bad "$name" "the oracle did not start" "$(tail -5 "$WORK/oracle.$name.err")"
        return
    fi
    local port
    read -r _ port <"$log"
    timeout 90 "$CLIENT" "$port" "$F/ca.pem" "$offsets" "$client_mode" "$connections" >"$out" 2>"$WORK/client.$name.err"
    local rc=$?
    kill "$pid" 2>/dev/null
    wait "$pid" 2>/dev/null
    local text
    text=$(cat "$out")
    if [ "$rc" -ne 0 ]; then
        bad "$name" "client exited $rc" "output: $text" "$(tail -3 "$WORK/client.$name.err")"
    elif ! matches "$want" <<<"$text"; then
        bad "$name" "output did not match /$want/" "got: $text" "oracle: $(tail -6 "$log")"
    elif [ "$want_log" != - ] && ! grep -Eq "$want_log" "$log"; then
        bad "$name" "the oracle's log did not match /$want_log/" "$(tail -8 "$log")"
    elif grep -q 'server error' "$log"; then
        bad "$name" "the oracle itself failed" "$(grep 'server error' "$log")"
    else
        note "$name"
    fi
}
# stapled13 <name> <mode:response> <client mode> <want> <oracle log regex or ->: the standard leaf and a 1-connection run.
stapled13() { oracle13 "$1" "$F/leaf_chain.pem" "$F/leaf.key" "$2" "$3" 0 1 "$4" "$5"; }
G="good:$F/c_good_sha1_certid.der"

stapled13 ocsp13_good_staple_is_accepted "$G" plain "$OK" 'status_request=0100000000'
stapled13 ocsp13_good_staple_is_accepted_when_required "$G" required "$OK" -
stapled13 ocsp13_hello_asks_for_ocsp_with_an_empty_responder_list "$G" plain "$OK" 'extensions=0,5,10,11,13,50,23,65281,43,51,45'
stapled13 ocsp13_no_staple_is_soft_by_default none plain "$OK_UNSTAPLED" 'client_finished ok'
stapled13 ocsp13_no_staple_when_required_stops none required "$(fails "$REFUSED_REQUIRED")" 'client alert 42'
stapled13 ocsp13_staple_on_the_issuers_entry_only_is_no_staple onissuer plain "$OK_UNSTAPLED" -
stapled13 ocsp13_staple_on_the_issuers_entry_only_when_required_stops onissuer required "$(fails "$REFUSED_REQUIRED")" 'client alert 42'
stapled13 ocsp13_unknown_entry_extension_is_ignored "withother:$F/c_good_sha1_certid.der" plain "$OK" -
stapled13 ocsp13_revoked_stops "good:$F/c_revoked.der" plain "$(fails "$REFUSED_REVOKED")" 'client alert 44'
stapled13 ocsp13_revoked_stops_when_required "good:$F/c_revoked.der" required "$(fails "$REFUSED_REVOKED")" 'client alert 44'
stapled13 ocsp13_unknown_status_stops "good:$F/c_unknown_status.der" plain "$(fails "$REFUSED_STAPLE")" 'client alert 42'
stapled13 ocsp13_wrong_certificate_stops "good:$F/c_wrong_certificate.der" plain "$(fails "$REFUSED_STAPLE")" 'client alert 42'
stapled13 ocsp13_bad_signature_stops "good:$F/c_bad_signature.der" plain "$(fails "$REFUSED_STAPLE")" 'client alert 42'
stapled13 ocsp13_critical_extension_stops "good:$F/c_critical_response_extension.der" plain "$(fails "$REFUSED_STAPLE")" 'client alert 42'
stapled13 ocsp13_sha1_signature_stops "good:$F/c_sha1_signature.der" plain "$(fails "$REFUSED_STAPLE")" 'client alert 42'
stapled13 ocsp13_unauthorised_responder_stops "good:$F/c_delegated_responder_no_eku.der" plain "$(fails "$REFUSED_STAPLE")" 'client alert 42'
stapled13 ocsp13_malformed_ocsp_inside_valid_framing_stops "garbage:$F/c_good_sha1_certid.der" plain "$(fails "$REFUSED_STAPLE")" 'client alert 42'
stapled13 ocsp13_delegated_responder_is_accepted "good:$F/c_good_delegated.der" plain "$OK" -
stapled13 ocsp13_not_yet_valid_stops "good:$F/c_not_yet_valid.der" plain "$(fails "$REFUSED_STAPLE")" 'client alert 42'
stapled13 ocsp13_unsuccessful_response_counts_as_no_staple "good:$F/c_status_3_no_body.der" plain "$OK_UNSTAPLED" -
stapled13 ocsp13_unsuccessful_response_when_required_stops "good:$F/c_status_3_no_body.der" required "$(fails "$REFUSED_REQUIRED")" 'client alert 42'
stapled13 ocsp13_empty_response_is_malformed "empty:$F/c_good_sha1_certid.der" plain "$(fails "$REFUSED_MALFORMED")" 'client alert 50'
for mode in badtype trailing truncated noextdata oneoctet dup; do
    stapled13 "ocsp13_${mode}_status_request_is_malformed" "$mode:$F/c_good_sha1_certid.der" plain "$(fails "$REFUSED_MALFORMED")" 'client alert 50'
done
stapled13 ocsp13_status_request_in_encrypted_extensions_is_refused "inee:$F/c_good_sha1_certid.der" plain "$(fails "$REFUSED_EXTENSION")" 'client alert 110'
stapled13 ocsp13_status_request_in_server_hello_is_refused "inhello:$F/c_good_sha1_certid.der" plain "$(fails "$REFUSED_EXTENSION")" -

# Seven bad staples, one after another, then a good one: no state leaks between connections.
oracle13 ocsp13_a_bad_staple_leaves_nothing_behind "$F/leaf_chain.pem" "$F/leaf.key" \
    "good:$F/c_revoked.der,good:$F/c_wrong_certificate.der,good:$F/c_good_sha1_certid.der" plain 0 3 \
    '^conn 1 error=[^\n]*\nconn 2 error=[^\n]*\nconn 3 ok resumed=false stapled=true ' -

# A certificate that says it must be stapled.
oracle13 ocsp13_must_staple_without_a_staple_stops "$F/muststaple_chain.pem" "$F/mustStaple.key" none plain 0 1 \
    "$(fails "$REFUSED_REQUIRED")" 'client alert 42'
oracle13 ocsp13_must_staple_with_a_staple_is_accepted "$F/muststaple_chain.pem" "$F/mustStaple.key" "good:$F/o_muststaple.der" plain 0 1 "$OK" -
oracle13 ocsp13_must_staple_with_a_revoked_staple_stops "$F/muststaple_chain.pem" "$F/mustStaple.key" "good:$F/c_revoked.der" plain 0 1 \
    "$(fails "$REFUSED_STAPLE")" 'client alert 42'
oracle13 ocsp13_must_staple_with_the_staple_on_another_certificate_stops "$F/muststaple_chain.pem" "$F/mustStaple.key" "good:$F/c_good_sha1_certid.der" plain 0 1 \
    "$(fails "$REFUSED_STAPLE")" 'client alert 42'

# Time: the client's clock decides whether a staple is fresh.
oracle13 ocsp13_expired_staple_stops "$F/leaf_chain.pem" "$F/leaf.key" "good:$F/c_good_sha1_certid.der" plain $((8 * 86400)) 1 \
    "$(fails "$REFUSED_STAPLE")" 'client alert 42'
oracle13 ocsp13_staple_from_the_future_stops "$F/leaf_chain.pem" "$F/leaf.key" "good:$F/c_good_sha1_certid.der" plain -86400 1 \
    "$(fails "$REFUSED_STAPLE")" 'client alert 42'

# --- openssl s_server -status_file -------------------------------------------------------------

if ! command -v openssl >/dev/null 2>&1; then
    skip "openssl s_server checks: openssl is not installed"
else
    serve() { # serve <name> <cert> <key> <s_server flags...>: sets SERVER_PORT, or returns 1.
        local name=$1 cert=$2 key=$3
        shift 3
        SERVER_PORT=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
        openssl s_server -accept "127.0.0.1:$SERVER_PORT" -cert "$cert" -key "$key" -www "$@" \
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
    # live <name> <client mode> <offsets> <connections> <want>
    live() {
        local name=$1 client_mode=$2 offsets=$3 connections=$4 want=$5 text rc
        text=$(timeout 90 "$CLIENT" "$SERVER_PORT" "$F/ca.pem" "$offsets" "$client_mode" "$connections" 2>"$WORK/client.$name.err")
        rc=$?
        if [ "$rc" -ne 0 ]; then
            bad "$name" "client exited $rc" "output: $text" "$(tail -3 "$WORK/client.$name.err")"
        elif ! matches "$want" <<<"$text"; then
            bad "$name" "output did not match /$want/" "got: $text" "server: $(tail -4 "$WORK/s_server.$NAME.log" 2>/dev/null)"
        else
            note "$name"
        fi
    }
    for version in tls1_3 tls1_2; do
        label=${version/tls1_/tls1}
        flags=("-$version" -num_tickets 0)
        NAME=${label}_good; serve "$NAME" "$F/leaf.pem" "$F/leaf.key" "${flags[@]}" -status_file "$F/o_good.der" && {
            live "ocsp_openssl_${label}_good_staple" plain 0 1 "$OK"
            live "ocsp_openssl_${label}_good_staple_when_required" required 0 1 "$OK"
            live "ocsp_openssl_${label}_good_staple_after_it_expired_stops" plain $((8 * 86400)) 1 "$(fails "$REFUSED_STAPLE")"
            live "ocsp_openssl_${label}_good_staple_before_it_was_made_stops" plain -86400 1 "$(fails "$REFUSED_STAPLE")"
        }
        NAME=${label}_delegated; serve "$NAME" "$F/leaf.pem" "$F/leaf.key" "${flags[@]}" -status_file "$F/o_delegated.der" && {
            live "ocsp_openssl_${label}_delegated_responder_staple" plain 0 1 "$OK"
        }
        NAME=${label}_nocerts; serve "$NAME" "$F/leaf.pem" "$F/leaf.key" "${flags[@]}" -status_file "$F/o_nocerts.der" && {
            live "ocsp_openssl_${label}_staple_signed_by_the_issuer_without_certificates" plain 0 1 "$OK"
        }
        NAME=${label}_revoked; serve "$NAME" "$F/revoked.pem" "$F/revoked.key" "${flags[@]}" -status_file "$F/o_revoked.der" && {
            live "ocsp_openssl_${label}_revoked_certificate_stops" plain 0 1 "$(fails "$REFUSED_REVOKED")"
            live "ocsp_openssl_${label}_revoked_certificate_stops_when_required" required 0 1 "$(fails "$REFUSED_REVOKED")"
        }
        NAME=${label}_wrong; serve "$NAME" "$F/leaf2.pem" "$F/leaf2.key" "${flags[@]}" -status_file "$F/o_good.der" && {
            live "ocsp_openssl_${label}_staple_for_another_certificate_stops" plain 0 1 "$(fails "$REFUSED_STAPLE")"
        }
        NAME=${label}_sha1; serve "$NAME" "$F/leaf.pem" "$F/leaf.key" "${flags[@]}" -status_file "$F/o_sha1.der" && {
            live "ocsp_openssl_${label}_sha1_signed_staple_stops" plain 0 1 "$(fails "$REFUSED_STAPLE")"
        }
        NAME=${label}_nonext; serve "$NAME" "$F/leaf.pem" "$F/leaf.key" "${flags[@]}" -status_file "$F/o_nonext.der" && {
            live "ocsp_openssl_${label}_staple_without_next_update_stops" plain 0 1 "$(fails "$REFUSED_STAPLE")"
        }
        NAME=${label}_unknown; serve "$NAME" "$F/stranger.pem" "$F/stranger.key" "${flags[@]}" -status_file "$F/o_unknown.der" && {
            live "ocsp_openssl_${label}_unknown_status_stops" plain 0 1 "$(fails "$REFUSED_STAPLE")"
        }
        NAME=${label}_none; serve "$NAME" "$F/leaf.pem" "$F/leaf.key" "${flags[@]}" && {
            live "ocsp_openssl_${label}_no_staple_is_soft_by_default" plain 0 1 "$OK_UNSTAPLED"
            live "ocsp_openssl_${label}_no_staple_when_required_stops" required 0 1 "$(fails "$REFUSED_REQUIRED")"
        }
        NAME=${label}_muststaple_none; serve "$NAME" "$F/mustStaple.pem" "$F/mustStaple.key" "${flags[@]}" && {
            live "ocsp_openssl_${label}_must_staple_certificate_without_a_staple_stops" plain 0 1 "$(fails "$REFUSED_REQUIRED")"
        }
        NAME=${label}_muststaple; serve "$NAME" "$F/mustStaple.pem" "$F/mustStaple.key" "${flags[@]}" -status_file "$F/o_muststaple.der" && {
            live "ocsp_openssl_${label}_must_staple_certificate_with_a_staple" plain 0 1 "$OK"
        }
    done

    # Resumption (TLS 1.3 only): a resumed session has no staple of its own; it must not
    # outlive the staple it was made under.
    NAME=resume; serve "$NAME" "$F/leaf.pem" "$F/leaf.key" -tls1_3 -num_tickets 1 -status_file "$F/o_good.der" && {
        live ocsp_openssl_resumption_after_a_stapled_handshake plain 0 3 \
            '^conn 1 ok resumed=false stapled=true .*\nconn 2 ok resumed=true stapled=false .*\nconn 3 ok resumed=true stapled=false '
    }
    NAME=resume_hour; serve "$NAME" "$F/leaf.pem" "$F/leaf.key" -tls1_3 -num_tickets 1 -status_file "$F/o_hour.der" && {
        live ocsp_openssl_resumption_stops_when_the_staple_would_have_expired plain 0,1800,7200 3 \
            '^conn 1 ok resumed=false stapled=true .*\nconn 2 ok resumed=true stapled=false .*\nconn 3 error='"$REFUSED_STAPLE"' alert=-1$'
    }
    NAME=resume_required; serve "$NAME" "$F/leaf.pem" "$F/leaf.key" -tls1_3 -num_tickets 1 -status_file "$F/o_good.der" && {
        live ocsp_openssl_resumed_session_is_not_shared_with_a_client_that_requires_a_staple required 0 2 \
            '^conn 1 ok resumed=false stapled=true .*\nconn 2 ok resumed=true stapled=false '
    }
fi

# --- the TLS 1.2 oracle ------------------------------------------------------------------------

# oracle12 <name> <mode> <response> <client mode> <want> <oracle log regex or ->
oracle12() {
    local name=$1 mode=$2 response=$3 client_mode=$4 want=$5 want_log=$6
    local log=$WORK/oracle.$name.log out=$WORK/client.$name.out
    OCSP_CHAIN=$F/leaf_chain.pem OCSP_KEY=$F/leaf.key OCSP_RESPONSE=$response \
        python3 apps/tls/tls12_server.py "$mode" p256 c02b x25519 >"$log" 2>"$WORK/oracle.$name.err" &
    local pid=$!
    pids+=("$pid")
    if ! await_line "$log" '^READY ' "$pid"; then
        bad "$name" "the oracle did not start" "$(tail -5 "$WORK/oracle.$name.err")"
        return
    fi
    local port
    read -r _ port _ <"$log"
    timeout 60 "$CLIENT" "$port" "$F/ca.pem" 0 "$client_mode" 1 >"$out" 2>"$WORK/client.$name.err"
    local rc=$?
    wait "$pid" 2>/dev/null
    local text
    text=$(cat "$out")
    if [ "$rc" -ne 0 ]; then
        bad "$name" "client exited $rc" "output: $text" "$(tail -3 "$WORK/client.$name.err")"
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
GOOD_RESPONSE=$F/c_good_sha1_certid.der
oracle12 ocsp12_good_staple_is_accepted staple_good "$GOOD_RESPONSE" plain "$OK" 'client_finished ok'
oracle12 ocsp12_good_staple_is_accepted_when_required staple_good "$GOOD_RESPONSE" required "$OK" -
oracle12 ocsp12_hello_asks_for_ocsp staple_good "$GOOD_RESPONSE" plain "$OK" 'extensions=0,5,10,11,13,50,23,65281,43,51,45'
oracle12 ocsp12_promise_without_a_message_is_no_staple staple_none "$GOOD_RESPONSE" plain "$OK_UNSTAPLED" 'client_finished ok'
oracle12 ocsp12_promise_without_a_message_when_required_stops staple_none "$GOOD_RESPONSE" required "$(fails "$REFUSED_REQUIRED")" 'client alert 42'
oracle12 ocsp12_revoked_stops staple_bad "$F/c_revoked.der" plain "$(fails "$REFUSED_REVOKED")" 'client alert 44'
oracle12 ocsp12_wrong_certificate_stops staple_bad "$F/c_wrong_certificate.der" plain "$(fails "$REFUSED_STAPLE")" 'client alert 42'
oracle12 ocsp12_bad_signature_stops staple_bad "$F/c_bad_signature.der" plain "$(fails "$REFUSED_STAPLE")" 'client alert 42'
oracle12 ocsp12_critical_extension_stops staple_bad "$F/c_critical_response_extension.der" plain "$(fails "$REFUSED_STAPLE")" 'client alert 42'
oracle12 ocsp12_unknown_status_stops staple_bad "$F/c_unknown_status.der" plain "$(fails "$REFUSED_STAPLE")" 'client alert 42'
oracle12 ocsp12_unsuccessful_response_is_no_staple staple_good "$F/c_status_3_no_body.der" plain "$OK_UNSTAPLED" -
oracle12 ocsp12_delegated_responder_is_accepted staple_good "$F/c_good_delegated.der" plain "$OK" -
oracle12 ocsp12_unpromised_status_message_is_refused staple_unpromised "$GOOD_RESPONSE" plain "$(fails "$REFUSED_ORDER")" 'client alert 10'
oracle12 ocsp12_status_message_after_the_key_exchange_is_refused staple_late "$GOOD_RESPONSE" plain "$(fails "$REFUSED_ORDER")" 'client alert 10'
oracle12 ocsp12_two_status_messages_are_refused staple_twice "$GOOD_RESPONSE" plain "$(fails "$REFUSED_ORDER")" 'client alert 10'
for mode in badtype empty trailing truncated; do
    oracle12 "ocsp12_${mode}_status_message_is_malformed" "staple_$mode" "$GOOD_RESPONSE" plain "$(fails "$REFUSED_MALFORMED")" 'client alert 50'
done
oracle12 ocsp12_status_request_with_data_in_the_server_hello_is_refused staple_ext_data "$GOOD_RESPONSE" plain "$(fails "$REFUSED_HELLO")" 'client alert 47'

echo
if [ $fail -eq 0 ]; then
    printf '\033[32mall %d ocsp checks passed\033[0m\n' "$pass"
else
    printf '\033[31m%d of %d ocsp checks FAILED\033[0m\n' "$fail" "$((pass + fail))"
fi
exit $fail
