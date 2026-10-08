#!/usr/bin/env bash
# Every check for the TLS 1.2 half of the client (`lib/tls.m31`, `lib/tls12.m31`,
# the legacy record functions of `lib/tls13_record.m31`, the ECDH section of
# `lib/ecdsa.m31`), against peers that are not this repository. Modelled on
# `apps/tls/test_tls.sh`.
#
#   bash apps/tls/test_tls12.sh
#
#   unit oracles        tls12_ecdh (P-256 and x25519 key agreement), tls12_prf
#                       (the RFC 5246 PRF, master secrets and key blocks, finished
#                       values) and tls12_record (TLS 1.2 record sealing and every
#                       way opening can fail, per AEAD) against the `cryptography`
#                       package and `hashlib`/`hmac`.
#   tls12_server.py     a TLS 1.2 server written from the RFCs that misbehaves on
#                       request. Every way the client must refuse is here with the
#                       error it must give and the alert it must send.
#   openssl s_server    the real thing: each of the six suites, both groups, the
#                       RSA schemes, and the servers that must be refused (no
#                       extended_master_secret, no suite in common, P-384 leaf).
#   badssl.com          skipped, never failed, without a network. Interop only.
#
# All peers are on loopback and disposable: a temporary directory, an ephemeral
# port, a readiness poll, cleanup on exit; no bare sleeps. The compiler is
# `LANGC` (default `./target/debug/m31c`, built with `cargo build`).
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

# Run harness $1 and oracle $2 (both with the arguments $4...), and require
# byte-identical output. $3 is what the lines are, for the pass message.
check() {
    local name=$1 oracle=$2 what=$3 label=$1
    shift 3
    [ $# -gt 0 ] && label="$name $*"
    if ! build "$name"; then
        return
    fi
    "$WORK/$name" "$@" >"$WORK/$name.got" 2>"$WORK/$name.err"
    local rc=$?
    if ! python3 "apps/tls/$oracle" "$@" >"$WORK/$name.want" 2>"$WORK/$name.oracle.err"; then
        bad "$label" "$oracle failed" "$(tail -5 "$WORK/$name.oracle.err")"
    elif [ $rc -ne 0 ]; then
        bad "$label" "$name exited $rc" "$(tail -5 "$WORK/$name.err")"
    elif cmp -s "$WORK/$name.got" "$WORK/$name.want"; then
        note "$label: $(wc -l <"$WORK/$name.got" | tr -d ' ') lines match $what"
    else
        bad "$label" "$(diff "$WORK/$name.got" "$WORK/$name.want" | head -12)"
    fi
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
    for f in lib/tls.m31 lib/tls12.m31 lib/tls13_record.m31 lib/ecdsa.m31 apps/tls/tls12_*.m31 apps/tls/t_tls_*.m31; do
        "$LANGC" fmt --check "$f" || echo "$f"
    done 2>&1
) && [ -z "$out" ]; then
    note "source is formatted"
else
    bad "source is not formatted (run: m31c fmt <file>)" "$out"
fi

# --- unit oracles ----------------------------------------------------------------

if ! python3 -c 'import cryptography' 2>/dev/null; then
    bad "oracles" "the 'cryptography' package is not importable -- pip install cryptography"
else
    check tls12_ecdh tls12_ecdh_oracle.py "OpenSSL ECDH (P-256 and x25519, ephemeral keys, every refused point)"
    check tls12_prf tls12_prf_oracle.py "hashlib/hmac (RFC 5246 PRF, extended master secret, key block, Finished)"
    for suite in chacha aes128 aes256; do
        check tls12_record tls12_record_oracle.py "the AEAD (sizes, types, sequences, every tamper and limit)" "$suite"
    done
fi

build t_tls_client || { echo; printf '\033[31m%d of %d tls 1.2 checks FAILED\033[0m\n' "$fail" "$((pass + fail))"; exit 1; }
CLIENT=$WORK/t_tls_client

# --- the scripted server matrix --------------------------------------------------

REFUSED_HELLO="the server's hello is not what was offered"
REFUSED_EXTENSION='the server used an extension that was not offered'
REFUSED_SCHEME='the server signed with a scheme that was not offered or does not fit its key'
REFUSED_SIGNATURE="the server's handshake signature is wrong"
REFUSED_FINISHED="the server's Finished does not verify"
REFUSED_ORDER='a handshake message arrived where it is not allowed'
REFUSED_MALFORMED='a handshake message is malformed'
REFUSED_TAG="a record's authentication tag did not verify"
REFUSED_ALERT='the server sent a fatal alert'
REFUSED_EMS='the TLS 1.2 server does not support extended_master_secret'
REFUSED_RENEGOTIATION="the server's renegotiation_info is not that of a first handshake"
REFUSED_VERSION='the server does not speak TLS 1.2 or 1.3'
REFUSED_SENTINEL="the server's random carries the TLS downgrade sentinel"
REFUSED_RECORD='the server broke a rule of the TLS record layer'
REFUSED_TRUNCATED='the connection ended without a close_notify'

# oracle <name> <mode> <auth> <suite hex> <group> <client mode> <client argument>
#        <expected client output, as a regex over the whole of it>
#        <expected client exit> <expected oracle log regex, or ->
oracle() {
    local name=$1 mode=$2 auth=$3 suite=$4 group=$5 client_mode=$6 argument=$7 want=$8 want_rc=$9 want_log=${10}
    local log=$WORK/oracle.$name.log out=$WORK/client.$name.out
    python3 apps/tls/tls12_server.py "$mode" "$auth" "$suite" "$group" >"$log" 2>"$WORK/oracle.$name.err" &
    local pid=$!
    pids+=("$pid")
    if ! await_line "$log" '^READY ' "$pid"; then
        bad "$name" "the oracle did not start" "$(tail -5 "$WORK/oracle.$name.err")"
        return
    fi
    local port pin
    read -r _ port pin <"$log"
    timeout 60 "$CLIENT" "$client_mode" 127.0.0.1 "$port" "$pin" "$argument" >"$out" 2>"$WORK/client.$name.err"
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

# accepted <name> <mode> <auth> <suite> <group> [log regex]: a good handshake and one echoed line.
accepted() {
    oracle "$1" "$2" "$3" "$4" "$5" echo hello '^line echo: hello\nclosed$' 0 "${6:-client_finished ok}"
}
# refused <name> <mode> <message> <alert> [auth [suite [group]]]: the handshake must fail with `message`, sending `alert`.
refused() {
    local name=$1 mode=$2 message=$3 alert=$4 auth=${5:-p256} suite=${6:-c02b} group=${7:-x25519}
    local log='-'
    [ "$alert" != - ] && log="client alert $alert"
    oracle "$name" "$mode" "$auth" "$suite" "$group" handshake '' "^error: $message\$" 1 "$log"
}

if python3 -c 'import cryptography' 2>/dev/null; then
    # The handshakes that are right: every suite, both groups, both key types.
    accepted suite_c02b_p256_x25519 ok p256 c02b x25519
    accepted suite_c02c_p256_x25519 ok p256 c02c x25519
    accepted suite_cca9_p256_x25519 ok p256 cca9 x25519
    accepted suite_c02f_rsa_x25519 ok rsa c02f x25519
    accepted suite_c030_rsa_x25519 ok rsa c030 x25519
    accepted suite_cca8_rsa_x25519 ok rsa cca8 x25519
    accepted group_p256_gcm128 ok p256 c02b p256
    accepted group_p256_gcm256_rsa ok rsa c030 p256
    accepted group_p256_chacha ok p256 cca9 p256
    accepted p384_leaf ok p384 c02c x25519
    accepted p384_leaf_sha256 ok_sha256_on_p384 p384 c02c x25519
    accepted p256_leaf_sha384 ok_sha384_on_p256 p256 c02b x25519
    accepted rsa_pkcs1 ok_pkcs1 rsa c02f x25519
    accepted rsa_pkcs1_sha512 ok_pkcs1_sha512 rsa c02f x25519
    accepted rsa_pss_sha512 ok_pss_sha512 rsa c02f x25519
    accepted no_renegotiation_info_is_allowed no_renegotiation_info p256 c02b x25519
    accepted points_list_with_uncompressed points_with_compressed p256 c02b x25519
    accepted finished_in_two_records finished_in_two_records p256 c02b x25519
    oracle explicit_nonce_is_taken_from_the_record wrong_explicit_nonce p256 c02b x25519 handshake '' '^connected\nclosed$' 0 'client_finished ok'
    oracle coalesced_flight coalesce p256 c02b x25519 echo hello '^line echo: hello\nclosed$' 0 -
    oracle split_messages split p256 c02b x25519 echo hello '^line echo: hello\nclosed$' 0 -
    oracle one_octet_segments dribble p256 c02b x25519 echo hello '^line echo: hello\nclosed$' 0 -
    oracle large_response big p256 c02b x25519 read_all $'go\n' '^read_all 100000\nclosed$' 0 'sent 100000 octets'
    oracle close_notify_ends_read_all close_notify p256 cca9 x25519 read_all $'go\n' '^read_all 9\nclosed$' 0 -
    oracle http_get ok p256 c02b x25519 get /index '^status 200\nbody 15\nhello over tls\n\nclosed$' 0 'line b.GET /index'
    oracle hello_request_is_declined hello_request p256 c02b x25519 echo hello '^line echo: hello\nclosed$' 0 'client alert 100'
    oracle offers_suites ok p256 c02b x25519 handshake '' '^connected\nclosed$' 0 \
        'offered suites=1303,1301,1302,cca9,cca8,c02b,c02f,c02c,c030 extensions=5,10,11,13,50,23,65281,43,51$'
    oracle offers_parameters ok p256 c02b x25519 handshake '' '^connected\nclosed$' 0 \
        'groups=001d,0017 sigalgs=0403,0503,0804,0805,0806,0401,0501,0601 versions=03040303 ec_point_formats=0100 renegotiation_info=00$'

    # The ones that are wrong: the hello.
    refused downgrade_sentinel downgrade "$REFUSED_SENTINEL" 47
    refused downgrade_sentinel_tls11 downgrade_tls11 "$REFUSED_SENTINEL" 47
    refused tls11_server version_tls11 "$REFUSED_VERSION" 70
    refused tls10_server version_tls10 "$REFUSED_VERSION" 70
    refused ssl3_server version_ssl3 "$REFUSED_VERSION" 70
    refused legacy_version_from_the_future version_future "$REFUSED_HELLO" 47
    refused no_extended_master_secret no_ems "$REFUSED_EMS" 40
    refused no_extension_block hello_no_extensions "$REFUSED_EMS" 40
    refused extended_master_secret_with_data ems_data "$REFUSED_HELLO" 47
    refused renegotiation_info_nonempty renego_nonempty "$REFUSED_RENEGOTIATION" 40
    refused renegotiation_info_empty_vector renego_empty_vector "$REFUSED_RENEGOTIATION" 40
    refused suite_from_tls13 suite_tls13 "$REFUSED_HELLO" 47
    refused suite_cbc_not_offered suite_cbc "$REFUSED_HELLO" 47
    refused compression_method compression "$REFUSED_HELLO" 47
    refused session_id_echoed echo_session_id "$REFUSED_HELLO" 47
    refused session_id_too_long long_session_id "$REFUSED_HELLO" 47
    refused extension_not_offered ext_unoffered "$REFUSED_EXTENSION" 110
    refused session_ticket_not_offered ext_session_ticket "$REFUSED_EXTENSION" 110
    refused server_name_with_data ext_sni_data "$REFUSED_EXTENSION" 110
    refused server_name_for_an_address ext_sni_empty "$REFUSED_EXTENSION" 110
    refused extension_twice ext_duplicate "$REFUSED_HELLO" 47
    refused no_uncompressed_point_format points_no_uncompressed "$REFUSED_HELLO" 47
    refused hello_trailing_octet hello_trailing "$REFUSED_MALFORMED" 50

    # Certificate, ServerKeyExchange, ServerHelloDone.
    refused certificate_in_tls13_format cert_13_format "$REFUSED_MALFORMED" 50
    refused certificate_empty cert_empty "the server's certificate is missing or unreadable" 42
    refused certificate_request cert_request 'the server asked for a client certificate' 40
    refused certificate_status_unrequested cert_status "$REFUSED_ORDER" 10
    refused done_before_exchange done_before_exchange "$REFUSED_ORDER" 10
    refused exchange_before_certificate exchange_before_certificate "$REFUSED_ORDER" 10
    refused no_exchange no_exchange "$REFUSED_ORDER" 10
    refused done_with_a_body done_body "$REFUSED_MALFORMED" 50
    refused zero_length_handshake_record empty_done_record "$REFUSED_ORDER" 10
    refused exchange_trailing_octet exchange_trailing "$REFUSED_MALFORMED" 50
    refused exchange_truncated exchange_truncated "$REFUSED_MALFORMED" 50
    refused x25519_zero_point zero_x25519 "$REFUSED_HELLO" 47
    refused x25519_short_point x25519_short "$REFUSED_HELLO" 47
    refused x25519_long_point x25519_long "$REFUSED_HELLO" 47
    refused group_not_offered group_p384 "$REFUSED_HELLO" 47
    refused group_unknown group_unknown "$REFUSED_HELLO" 47
    refused curve_explicit_prime curve_explicit_prime "$REFUSED_HELLO" 47
    refused curve_explicit_char2 curve_explicit_char2 "$REFUSED_HELLO" 47
    refused p256_off_curve p256_off_curve "$REFUSED_HELLO" 47 p256 c02b p256
    refused p256_compressed_point p256_compressed "$REFUSED_HELLO" 47 p256 c02b p256
    refused p256_point_at_infinity p256_infinity "$REFUSED_HELLO" 47 p256 c02b p256
    refused p256_wrong_prefix p256_wrong_prefix "$REFUSED_HELLO" 47 p256 c02b p256
    refused p256_coordinate_not_reduced p256_not_reduced "$REFUSED_HELLO" 47 p256 c02b p256

    # The signature over the key exchange.
    refused bad_signature bad_signature "$REFUSED_SIGNATURE" 51
    refused bad_signature_p384 bad_signature "$REFUSED_SIGNATURE" 51 p384 c02c
    refused bad_signature_rsa bad_signature "$REFUSED_SIGNATURE" 51 rsa c02f
    refused signature_empty signature_empty "$REFUSED_SIGNATURE" 51
    refused signature_truncated signature_truncated "$REFUSED_SIGNATURE" 51
    refused signature_over_swapped_randoms sign_swapped_randoms "$REFUSED_SIGNATURE" 51
    refused signature_without_randoms sign_without_randoms "$REFUSED_SIGNATURE" 51
    refused scheme_ed25519 scheme_ed25519 "$REFUSED_SCHEME" 47
    refused scheme_sha1 scheme_sha1 "$REFUSED_SCHEME" 47
    refused scheme_sha1_rsa scheme_sha1 "$REFUSED_SCHEME" 47 rsa c02f
    refused scheme_md5 scheme_md5 "$REFUSED_SCHEME" 47
    refused scheme_unknown scheme_unknown "$REFUSED_SCHEME" 47
    refused scheme_rsa_for_an_ec_key scheme_rsa_on_ec "$REFUSED_SCHEME" 47
    refused scheme_ecdsa_for_an_rsa_key scheme_rsa_on_ec "$REFUSED_SCHEME" 47 rsa c02f
    refused scheme_ecdsa_on_an_rsa_suite scheme_ecdsa_on_rsa_suite "$REFUSED_SCHEME" 47 rsa c02f
    refused scheme_pss_on_an_ecdsa_suite scheme_pss_on_ecdsa_suite "$REFUSED_SCHEME" 47
    refused scheme_pkcs1_for_an_ec_key ok_pkcs1 "$REFUSED_SCHEME" 47

    # ChangeCipherSpec, Finished and the record layer.
    refused no_change_cipher_spec no_ccs "$REFUSED_ORDER" 10
    refused change_cipher_spec_wrong_body ccs_body "$REFUSED_ORDER" 10
    refused change_cipher_spec_two_octets ccs_two_octets "$REFUSED_ORDER" 10
    refused finished_unprotected finished_unprotected "$REFUSED_TAG" 20
    refused finished_bad_record bad_record "$REFUSED_TAG" 20
    refused finished_short_record short_record "$REFUSED_TAG" 20
    refused finished_wrong_verify_data bad_finished "$REFUSED_FINISHED" 51
    refused finished_client_label finished_client_label "$REFUSED_FINISHED" 51
    refused finished_over_the_wrong_transcript finished_without_client_finished "$REFUSED_FINISHED" 51
    refused finished_too_long finished_long "$REFUSED_MALFORMED" 50
    refused finished_too_short finished_short "$REFUSED_MALFORMED" 50
    refused finished_after_an_empty_record finished_empty_record_first "$REFUSED_ORDER" 10
    refused application_data_before_finished app_data_before_finished "$REFUSED_ORDER" 10
    refused alert_instead_of_finished alert_instead_of_finished "$REFUSED_ALERT" -
    refused bad_finished_chacha bad_finished "$REFUSED_FINISHED" 51 p256 cca9
    refused bad_record_chacha bad_record "$REFUSED_TAG" 20 p256 cca9
    refused bad_finished_aes256 bad_finished "$REFUSED_FINISHED" 51 p256 c02c

    # After the handshake.
    oracle warning_alert_is_fatal warning_alert p256 c02b x25519 echo hello "^error: $REFUSED_ALERT\$" 1 -
    oracle peer_alert peer_alert p256 c02b x25519 read_all '' "^error: $REFUSED_ALERT\$" 1 -
    oracle unexpected_handshake unexpected_handshake p256 c02b x25519 echo hello "^error: $REFUSED_ORDER\$" 1 -
    oracle replayed_record replay_record p256 c02b x25519 read_all '' "^error: $REFUSED_TAG\$" 1 'client alert 20'
    oracle oversized_record oversized_record p256 c02b x25519 read_all $'go\n' "^error: $REFUSED_RECORD\$" 1 'client alert 22'
    oracle truncated_response truncate p256 c02b x25519 read_all $'go\n' "^error: $REFUSED_TRUNCATED\$" 1 'closing without close_notify'
    oracle truncated_response_chacha truncate p256 cca9 x25519 read_all $'go\n' "^error: $REFUSED_TRUNCATED\$" 1 'closing without close_notify'
fi

# --- openssl s_server ------------------------------------------------------------

if ! command -v openssl >/dev/null 2>&1; then
    skip "openssl s_server checks: openssl is not installed"
else
    make_cert() { # make_cert <name> <openssl req key flags...>
        local name=$1
        shift
        openssl req -x509 -newkey "$@" -nodes -keyout "$WORK/$name.key" -out "$WORK/$name.pem" \
            -subj /CN=localhost -days 1 >/dev/null 2>&1
        openssl x509 -in "$WORK/$name.pem" -pubkey -noout | openssl pkey -pubin -outform der 2>/dev/null \
            | openssl dgst -sha256 -binary | od -An -v -tx1 | tr -d ' \n'
    }
    EC_PIN=$(make_cert ec ec -pkeyopt ec_paramgen_curve:prime256v1)
    RSA_PIN=$(make_cert rsa rsa:2048)
    P384_PIN=$(make_cert p384 ec -pkeyopt ec_paramgen_curve:secp384r1)
    if [ -z "$EC_PIN" ] || [ -z "$RSA_PIN" ] || [ -z "$P384_PIN" ]; then
        skip "openssl s_server checks: cannot make certificates"
    else
        # serve <name> <cert name> <s_server flags...>: sets SERVER_PORT, or returns 1.
        serve() {
            local name=$1 cert=$2
            shift 2
            SERVER_PORT=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
            openssl s_server -accept "127.0.0.1:$SERVER_PORT" -cert "$WORK/$cert.pem" -key "$WORK/$cert.key" "$@" \
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

        # openssl <name> <cert name> <pin> <name of the suite> <s_server flags...>: GET /, then an echo.
        # The server is restricted to one TLS 1.2 suite, so a pass means that suite ran.
        suite_test() {
            local name=$1 cert=$2 pin=$3 cipher=$4
            shift 4
            if serve "$name" "$cert" -tls1_2 -cipher "$cipher" "$@" -www; then
                live "openssl_$name" '^status 200\nbody [0-9]+\n(.|\n)*closed$' 0 get 127.0.0.1 "$SERVER_PORT" "$pin" /
                kill "$SERVER_PID" 2>/dev/null
            fi
        }

        suite_test ecdsa_aes128 ec "$EC_PIN" ECDHE-ECDSA-AES128-GCM-SHA256
        suite_test ecdsa_aes256 ec "$EC_PIN" ECDHE-ECDSA-AES256-GCM-SHA384
        suite_test ecdsa_chacha ec "$EC_PIN" ECDHE-ECDSA-CHACHA20-POLY1305
        suite_test rsa_aes128 rsa "$RSA_PIN" ECDHE-RSA-AES128-GCM-SHA256
        suite_test rsa_aes256 rsa "$RSA_PIN" ECDHE-RSA-AES256-GCM-SHA384
        suite_test rsa_chacha rsa "$RSA_PIN" ECDHE-RSA-CHACHA20-POLY1305
        # The P-256 group is the only one the server will do.
        suite_test ecdsa_aes128_group_p256 ec "$EC_PIN" ECDHE-ECDSA-AES128-GCM-SHA256 -groups P-256
        suite_test rsa_chacha_group_p256 rsa "$RSA_PIN" ECDHE-RSA-CHACHA20-POLY1305 -groups P-256
        suite_test rsa_aes256_group_p256 rsa "$RSA_PIN" ECDHE-RSA-AES256-GCM-SHA384 -groups P-256
        # The RSA signature schemes.
        suite_test rsa_pkcs1_sha256 rsa "$RSA_PIN" ECDHE-RSA-AES128-GCM-SHA256 -sigalgs RSA+SHA256
        suite_test rsa_pkcs1_sha384 rsa "$RSA_PIN" ECDHE-RSA-AES128-GCM-SHA256 -sigalgs RSA+SHA384
        suite_test rsa_pss_sha256 rsa "$RSA_PIN" ECDHE-RSA-AES128-GCM-SHA256 -sigalgs RSA-PSS+SHA256
        suite_test rsa_pss_sha384 rsa "$RSA_PIN" ECDHE-RSA-AES128-GCM-SHA256 -sigalgs RSA-PSS+SHA384
        suite_test ecdsa_sha384 ec "$EC_PIN" ECDHE-ECDSA-AES128-GCM-SHA256 -sigalgs ECDSA+SHA384

        if serve echo ec -tls1_2 -cipher ECDHE-ECDSA-AES128-GCM-SHA256 -rev; then
            live openssl_echo '^line olleh\nclosed$' 0 echo 127.0.0.1 "$SERVER_PORT" "$EC_PIN" hello
            live openssl_echo_twice '^line dlrow\nline dlrow\nclosed$' 0 twice 127.0.0.1 "$SERVER_PORT" "$EC_PIN" world
            kill "$SERVER_PID" 2>/dev/null
        fi

        # What must be refused.
        if serve wrong_pin ec -tls1_2 -www; then
            live openssl_wrong_pin '^error: the server.s key is not the pinned key$' 1 \
                handshake 127.0.0.1 "$SERVER_PORT" 0000000000000000000000000000000000000000000000000000000000000000
            kill "$SERVER_PID" 2>/dev/null
        fi
        if serve no_ems ec -tls1_2 -no_ems -www; then
            live openssl_no_extended_master_secret "^error: $REFUSED_EMS\$" 1 handshake 127.0.0.1 "$SERVER_PORT" "$EC_PIN"
            kill "$SERVER_PID" 2>/dev/null
        fi
        # AES-CBC and RSA key exchange are never offered, so a server that only does those has no suite in common.
        if serve cbc_only rsa -tls1_2 -cipher ECDHE-RSA-AES128-SHA -www; then
            live openssl_cbc_only '^error: ' 1 handshake 127.0.0.1 "$SERVER_PORT" "$RSA_PIN"
            kill "$SERVER_PID" 2>/dev/null
        fi
        if serve rsa_key_exchange_only rsa -tls1_2 -cipher AES128-GCM-SHA256 -www; then
            live openssl_rsa_key_exchange_only '^error: ' 1 handshake 127.0.0.1 "$SERVER_PORT" "$RSA_PIN"
            kill "$SERVER_PID" 2>/dev/null
        fi
        # A P-384 leaf: the client does not advertise secp384r1 (the stance in `lib/tls.m31`), so an OpenSSL
        # server with only such a key has no certificate it may use. The scripted server above covers the signature.
        if serve p384_leaf p384 -tls1_2 -www; then
            live openssl_p384_leaf_is_refused '^error: ' 1 handshake 127.0.0.1 "$SERVER_PORT" "$P384_PIN"
            kill "$SERVER_PID" 2>/dev/null
        fi
        # A TLS 1.1 server: the client does not go below 1.2.
        if serve tls11 rsa -tls1_1 -cipher 'ALL:@SECLEVEL=0' -www; then
            live openssl_tls11_only '^error: ' 1 handshake 127.0.0.1 "$SERVER_PORT" "$RSA_PIN"
            kill "$SERVER_PID" 2>/dev/null
        fi
    fi
fi

# --- badssl.com -------------------------------------------------------------------

badssl() { # badssl <name> <host> <port> <expected output regex> <expected exit>
    local name=$1 host=$2 port=$3 want=$4 want_rc=$5
    command -v openssl >/dev/null 2>&1 || { skip "$name: openssl is not installed"; return; }
    local chain=$WORK/$name.pem key=$WORK/$name.key
    if ! timeout 20 openssl s_client -connect "$host:$port" -servername "$host" </dev/null >"$chain" 2>/dev/null; then
        skip "$name: no network, or $host:$port does not answer"
        return
    fi
    openssl x509 -in "$chain" -pubkey -noout 2>/dev/null | openssl pkey -pubin -outform der >"$key" 2>/dev/null
    if [ ! -s "$key" ]; then
        skip "$name: could not read its certificate"
        return
    fi
    local pin text rc
    pin=$(openssl dgst -sha256 -binary <"$key" | od -An -v -tx1 | tr -d ' \n')
    text=$(timeout 60 "$CLIENT" handshake "$host" "$port" "$pin" 2>"$WORK/$name.err")
    rc=$?
    if [ "$rc" -eq "$want_rc" ] && matches "$want" <<<"$text"; then
        note "$name: $(printf '%s' "$text" | head -1 | cut -c1-100)"
    else
        bad "$name" "client exited $rc, wanted $want_rc" "$(printf '%s' "$text" | head -3 | cut -c1-200)" "$(tail -3 "$WORK/$name.err")"
    fi
}
# These hosts run an OpenSSL old enough to have no extended_master_secret (and no TLS 1.3), so the client
# refuses them on purpose: the TLS 1.2 handshake is only as safe as the master secret, and without the
# extension the triple-handshake attack applies. An interop check that fails closed.
badssl badssl_tls12_only_has_no_ems tls-v1-2.badssl.com 1012 "^error: $REFUSED_EMS\$" 1
badssl badssl_rsa2048_has_no_ems rsa2048.badssl.com 443 "^error: $REFUSED_EMS\$" 1
badssl badssl_ecc256_has_no_ems ecc256.badssl.com 443 "^error: $REFUSED_EMS\$" 1
badssl badssl_tls11_only tls-v1-1.badssl.com 1011 '^error: ' 1

echo
if [ $fail -eq 0 ]; then
    printf '\033[32mall %d tls 1.2 checks passed\033[0m\n' "$pass"
else
    printf '\033[31m%d of %d tls 1.2 checks FAILED\033[0m\n' "$fail" "$((pass + fail))"
fi
exit $fail
