#!/usr/bin/env bash
# Every check for TLS 1.3 client certificates in the client (`lib/clientcert.m31`
# and the client-authentication code of `lib/tls.m31`), against peers that are not
# this repository. Modelled on `apps/tls/test_resume.sh`.
#
#   bash apps/tls/test_clientcert.sh
#
#   clientcert_server.py  a TLS 1.3 server written from RFC 8446 that sends a
#                         CertificateRequest, checks the client's Certificate,
#                         CertificateVerify (content, scheme, signature) and Finished
#                         itself, and misbehaves on request: every malformed or
#                         unsatisfiable request the client must refuse is here with
#                         the error it must give and the alert it must send.
#   openssl s_server      the real thing: `-Verify` (a certificate is required) and
#                         `-verify` (asked for), ECDSA P-256 and Ed25519 identities,
#                         a chain with an intermediate, `-client_sigalgs` limits, an
#                         untrusted CA, an expired certificate, and resumption of a
#                         session that was made with a client certificate.
#
# The client is `t_clientcert_client.m31`: N connections over one session cache, each
# presenting the next identity of a comma-separated list. Fixtures (CA, intermediate,
# identities, and the broken key and chain files the loader must refuse) are made
# by the `cryptography` package. Every peer is on loopback and disposable. The
# compiler is `LANGC` (default `./target/debug/m31c`, built with `cargo build`).
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
    for f in lib/tls.m31 lib/clientcert.m31 lib/http.m31 apps/tls/t_clientcert_client.m31 apps/tls/t_chain_client.m31; do
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
build t_clientcert_client || { printf '\033[31m%d of %d clientcert checks FAILED\033[0m\n' "$fail" "$((pass + fail))"; exit 1; }
CLIENT=$WORK/t_clientcert_client

# --- fixtures ------------------------------------------------------------------

F=$WORK/fx
mkdir -p "$F"
python3 - "$F" <<'PYEOF'
import datetime, sys
from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec, ed25519, rsa
from cryptography.x509.oid import ExtendedKeyUsageOID, NameOID

out = sys.argv[1]
now = datetime.datetime.now(datetime.timezone.utc)
day = datetime.timedelta(days=1)
PEM = serialization.Encoding.PEM

def name(common_name):
    return x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, common_name)])

def certificate(subject, subject_key, issuer, issuer_key, serial, authority=False,
                before=now - day, after=now + 2 * day, digest=hashes.SHA256()):
    builder = (x509.CertificateBuilder().subject_name(name(subject)).issuer_name(name(issuer))
               .public_key(subject_key.public_key()).serial_number(serial)
               .not_valid_before(before).not_valid_after(after)
               .add_extension(x509.BasicConstraints(ca=authority, path_length=None), critical=True))
    if authority:
        builder = builder.add_extension(x509.KeyUsage(False, False, False, False, False, True, True, False, False), critical=True)
    else:
        builder = builder.add_extension(x509.KeyUsage(True, False, False, False, False, False, False, False, False), critical=True)
        builder = builder.add_extension(x509.ExtendedKeyUsage([ExtendedKeyUsageOID.CLIENT_AUTH]), critical=False)
    return builder.sign(issuer_key, digest)

def pem_of(certificates):
    return b"".join(c.public_bytes(PEM) for c in certificates)

def pkcs8(key, encryption=serialization.NoEncryption()):
    return key.private_bytes(PEM, serialization.PrivateFormat.PKCS8, encryption)

def write(file_name, data):
    open(out + "/" + file_name, "wb").write(data)

ca_key = ec.generate_private_key(ec.SECP256R1())
ca = certificate("cc ca", ca_key, "cc ca", ca_key, 1, authority=True)
write("ca.pem", pem_of([ca]))
middle_key = ec.generate_private_key(ec.SECP256R1())
middle = certificate("cc intermediate", middle_key, "cc ca", ca_key, 2, authority=True)

p256_key = ec.generate_private_key(ec.SECP256R1())
p256 = certificate("client-p256", p256_key, "cc ca", ca_key, 10)
write("p256.pem", pem_of([p256])); write("p256.key", pkcs8(p256_key))
write("p256sec1.key", p256_key.private_bytes(PEM, serialization.PrivateFormat.TraditionalOpenSSL, serialization.NoEncryption()))

ed_key = ed25519.Ed25519PrivateKey.generate()
ed = certificate("client-ed25519", ed_key, "cc ca", ca_key, 11)
write("ed25519.pem", pem_of([ed])); write("ed25519.key", pkcs8(ed_key))

deep_key = ec.generate_private_key(ec.SECP256R1())
deep = certificate("client-deep", deep_key, "cc intermediate", middle_key, 12)
write("deep.pem", pem_of([deep, middle])); write("deep.key", pkcs8(deep_key))
write("deep_without_intermediate.pem", pem_of([deep]))

rogue_key = ec.generate_private_key(ec.SECP256R1())
rogue_ca = certificate("rogue ca", rogue_key, "rogue ca", rogue_key, 20, authority=True)
rogue_leaf_key = ec.generate_private_key(ec.SECP256R1())
rogue = certificate("client-rogue", rogue_leaf_key, "rogue ca", rogue_key, 21)
write("rogue.pem", pem_of([rogue])); write("rogue.key", pkcs8(rogue_leaf_key))

expired_key = ec.generate_private_key(ec.SECP256R1())
expired = certificate("client-expired", expired_key, "cc ca", ca_key, 30,
                      before=now - 10 * day, after=now - 5 * day)
write("expired.pem", pem_of([expired])); write("expired.key", pkcs8(expired_key))

# Things the loader must refuse.
write("encrypted.key", pkcs8(p256_key, serialization.BestAvailableEncryption(b"hunter2")))
rsa_key = rsa.generate_private_key(65537, 2048)
write("rsa.key", pkcs8(rsa_key))
p384_key = ec.generate_private_key(ec.SECP384R1())
write("p384.key", pkcs8(p384_key))
write("garbage.pem", b"this is not PEM at all\n")
write("empty.key", b"")
lines = p256.public_bytes(PEM).decode().splitlines()
write("truncated.pem", ("\n".join(lines[:3]) + "\n").encode())
write("badder.pem", b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n")
write("keyasciichain.pem", p256.public_bytes(PEM) + b"-----BEGIN CERTIFICATE-----\nnot base64!\n-----END CERTIFICATE-----\n")

# The server's own key and certificate for openssl s_server, and its pin.
server_key = ec.generate_private_key(ec.SECP256R1())
server = x509.CertificateBuilder().subject_name(name("localhost")).issuer_name(name("localhost")) \
    .public_key(server_key.public_key()).serial_number(40).not_valid_before(now - day).not_valid_after(now + 2 * day) \
    .add_extension(x509.SubjectAlternativeName([x509.DNSName("localhost")]), critical=False) \
    .sign(server_key, hashes.SHA256())
write("server.pem", pem_of([server])); write("server.key", pkcs8(server_key))
PYEOF
if [ ! -s "$F/deep.key" ]; then
    bad "fixtures" "python could not make the test certificates"
    exit 1
fi
SERVER_PIN=$(openssl x509 -in "$F/server.pem" -pubkey -noout 2>/dev/null | openssl pkey -pubin -outform der 2>/dev/null \
    | openssl dgst -sha256 -binary | od -An -v -tx1 | tr -d ' \n')

# --- the loader ----------------------------------------------------------------

# loader <name> <chain file> <key file> <output regex>
loader() {
    local name=$1 chain=$2 key=$3 want=$4 text
    text=$(timeout 30 "$CLIENT" 1 "$(printf 'ab%.0s' {1..32})" "$chain" "$key" 1 2>"$WORK/loader.$name.err")
    if matches "$want" <<<"$text"; then
        note "$name"
    else
        bad "$name" "output did not match /$want/" "got: $text" "$(tail -2 "$WORK/loader.$name.err")"
    fi
}
LOADED='^identity Identity\((ECDSA P-256|Ed25519) sha256:([0-9a-f]{2}:){31}[0-9a-f]{2}\)\n'
loader clientcert_loads_p256 "$F/p256.pem" "$F/p256.key" "$LOADED"
loader clientcert_loads_sec1_p256_key "$F/p256.pem" "$F/p256sec1.key" "$LOADED"
loader clientcert_loads_ed25519 "$F/ed25519.pem" "$F/ed25519.key" '^identity Identity\(Ed25519 '
loader clientcert_loads_chain_with_intermediate "$F/deep.pem" "$F/deep.key" "$LOADED"
loader clientcert_refuses_a_key_that_is_not_the_certificates "$F/p256.pem" "$F/ed25519.key" '^identity error=the private key is not the one the first certificate certifies$'
loader clientcert_refuses_a_swapped_curve_key "$F/ed25519.pem" "$F/p256.key" '^identity error=the private key is not the one'
loader clientcert_refuses_an_encrypted_key "$F/p256.pem" "$F/encrypted.key" '^identity error=the private key is passphrase-protected$'
loader clientcert_refuses_an_rsa_key "$F/p256.pem" "$F/rsa.key" '^identity error=the private key is not Ed25519 or ECDSA P-256$'
loader clientcert_refuses_a_p384_key "$F/p256.pem" "$F/p384.key" '^identity error=the private key is not Ed25519 or ECDSA P-256$'
loader clientcert_refuses_an_empty_key_file "$F/p256.pem" "$F/empty.key" '^identity error=no usable private key in the key file$'
loader clientcert_refuses_a_key_file_that_is_a_certificate "$F/p256.pem" "$F/p256.pem" '^identity error=(no usable private key in the key file|the private key is not)'
loader clientcert_refuses_a_chain_that_is_not_pem "$F/garbage.pem" "$F/p256.key" '^identity error=the certificate chain is not PEM certificates$'
loader clientcert_refuses_a_truncated_pem_block "$F/truncated.pem" "$F/p256.key" '^identity error=the certificate chain is not PEM certificates$'
loader clientcert_refuses_a_certificate_that_is_not_x509 "$F/badder.pem" "$F/p256.key" '^identity error=a certificate in the chain could not be parsed$'
loader clientcert_refuses_a_chain_with_a_bad_block "$F/keyasciichain.pem" "$F/p256.key" '^identity error=the certificate chain is not PEM certificates$'
loader clientcert_refuses_a_missing_chain_file "$F/nowhere.pem" "$F/p256.key" '^identity error=a certificate or key file could not be read$'
loader clientcert_refuses_a_missing_key_file "$F/p256.pem" "$F/nowhere.key" '^identity error=a certificate or key file could not be read$'

# No key bytes in anything the loader prints.
secret=$(python3 - "$F/p256.key" <<'PYEOF'
import base64, re, sys
text = open(sys.argv[1]).read()
body = "".join(line for line in text.splitlines() if not line.startswith("-----"))
print(base64.b64decode(body).hex()[-40:])
PYEOF
)
shown=$(timeout 30 "$CLIENT" 1 "$(printf 'ab%.0s' {1..32})" "$F/p256.pem" "$F/p256.key" 1 2>&1)
if [ -n "$secret" ] && ! grep -qi -- "$secret" <<<"$shown"; then
    note "clientcert_never_prints_key_material"
else
    bad "clientcert_never_prints_key_material" "$shown"
fi

# --- the Python oracle ----------------------------------------------------------

REFUSED_HELLO="the server's hello is not what was offered"
REFUSED_MALFORMED='a handshake message is malformed'
REFUSED_ORDER='a handshake message arrived where it is not allowed'
REFUSED_ASKED='the server asked for a client certificate'
REFUSED_SCHEME="the server accepts no signature scheme the client certificate's key signs with"

# scenario <name> <modes> <chains> <keys> <connections> <output regex> <oracle log regex or -> <log regex that must NOT match or ->
# The client's output (without its `identity` lines) is matched as a whole; the oracle's log line by line.
scenario() {
    local name=$1 modes=$2 chains=$3 keys=$4 connections=$5 want=$6 want_log=$7 deny_log=$8
    local log=$WORK/oracle.$name.log out=$WORK/client.$name.out
    python3 apps/tls/clientcert_server.py "$modes" >"$log" 2>"$WORK/oracle.$name.err" &
    local pid=$!
    pids+=("$pid")
    if ! await_line "$log" '^READY ' "$pid"; then
        bad "$name" "the oracle did not start" "$(tail -5 "$WORK/oracle.$name.err")"
        return
    fi
    local port pin
    read -r _ port pin <"$log"
    timeout 90 "$CLIENT" "$port" "$pin" "$chains" "$keys" "$connections" >"$out" 2>"$WORK/client.$name.err"
    local rc=$?
    wait "$pid" 2>/dev/null
    local text
    text=$(grep -v '^identity ' "$out")
    if [ "$rc" -ne 0 ]; then
        bad "$name" "client exited $rc" "output: $text" "$(tail -3 "$WORK/client.$name.err")"
    elif ! matches "$want" <<<"$text"; then
        bad "$name" "output did not match /$want/" "got: $text" "oracle: $(tail -6 "$log")"
    elif [ "$want_log" != - ] && ! grep -Eq "$want_log" "$log"; then
        bad "$name" "the oracle's log did not match /$want_log/" "$(tail -12 "$log")"
    elif [ "$deny_log" != - ] && grep -Eq "$deny_log" "$log"; then
        bad "$name" "the oracle's log matched /$deny_log/" "$(grep -E "$deny_log" "$log" | head -3)"
    elif grep -q 'server error' "$log"; then
        bad "$name" "the oracle itself failed" "$(grep 'server error' "$log")"
    else
        note "$name"
    fi
}

GOOD='verify=ok'
BADLOG='verify=BAD|client_finished BAD'
P256="$F/p256.pem"; P256K="$F/p256.key"
ED="$F/ed25519.pem"; EDK="$F/ed25519.key"

scenario clientcert_p256_is_verified_by_the_oracle good "$P256" "$P256K" 1 \
    '^conn 1 ok resumed=false status=HTTP/1\.[01] 200 [Oo][Kk]$' \
    '1 client_verify scheme=0x0403 verify=ok' "$BADLOG"
scenario clientcert_ed25519_is_verified_by_the_oracle good "$ED" "$EDK" 1 \
    '^conn 1 ok resumed=false status=HTTP/1\.[01] 200 [Oo][Kk]$' \
    '1 client_verify scheme=0x0807 verify=ok' "$BADLOG"
scenario clientcert_finished_follows_client_authentication good "$P256" "$P256K" 1 \
    '^conn 1 ok ' '1 client_finished ok' 'client_finished BAD'
scenario clientcert_sends_exactly_the_leaf good "$P256" "$P256K" 1 \
    '^conn 1 ok ' '1 client_certificates count=1' -
scenario clientcert_sends_the_whole_chain good "$F/deep.pem" "$F/deep.key" 1 \
    '^conn 1 ok ' '1 client_certificates count=2' "$BADLOG"
scenario clientcert_leaf_is_the_one_configured good "$ED" "$EDK" 1 \
    '^conn 1 ok ' '1 client_leaf subject=CN=client-ed25519' -
scenario clientcert_p256_when_only_p256_is_offered p256only "$P256" "$P256K" 1 \
    '^conn 1 ok ' '1 client_verify scheme=0x0403 verify=ok' "$BADLOG"
scenario clientcert_ed25519_when_only_ed25519_is_offered ed25519only "$ED" "$EDK" 1 \
    '^conn 1 ok ' '1 client_verify scheme=0x0807 verify=ok' "$BADLOG"
scenario clientcert_authorities_list_is_ignored cas "$P256" "$P256K" 1 \
    '^conn 1 ok ' '1 client_verify scheme=0x0403 verify=ok' "$BADLOG"
scenario clientcert_no_request_sends_no_certificate none "$P256" "$P256K" 1 \
    '^conn 1 ok resumed=false' '1 client_finished ok' 'client_certificates|client_verify'

# A client that has no certificate does not send an empty one: it stops.
scenario clientcert_without_identity_refuses_a_request good none none 1 \
    "^conn 1 error=$REFUSED_ASKED alert=-1\$" 'client alert 40' 'client_certificates'
scenario clientcert_without_identity_and_no_request_is_fine none none none 1 \
    '^conn 1 ok ' '1 client_finished ok' -

# A key that cannot sign what the server accepts: refused before anything is sent.
scenario clientcert_p256_key_but_server_wants_only_ed25519 ed25519only "$P256" "$P256K" 1 \
    "^conn 1 error=$REFUSED_SCHEME alert=-1\$" 'client alert 40' 'client_certificates'
scenario clientcert_ed25519_key_but_server_wants_only_p256 p256only "$ED" "$EDK" 1 \
    "^conn 1 error=$REFUSED_SCHEME alert=-1\$" 'client alert 40' 'client_certificates'
scenario clientcert_no_scheme_in_common nomatch "$P256" "$P256K" 1 \
    "^conn 1 error=$REFUSED_SCHEME alert=-1\$" 'client alert 40' 'client_certificates'

# Hostile CertificateRequests.
scenario clientcert_refuses_a_nonempty_request_context ctx "$P256" "$P256K" 1 \
    "^conn 1 error=$REFUSED_HELLO alert=-1\$" 'client alert 47' 'client_certificates'
scenario clientcert_refuses_a_request_without_signature_algorithms nosigalgs "$P256" "$P256K" 1 \
    "^conn 1 error=$REFUSED_HELLO alert=-1\$" 'client alert 47' 'client_certificates'
scenario clientcert_refuses_duplicate_signature_algorithms dupext "$P256" "$P256K" 1 \
    "^conn 1 error=$REFUSED_HELLO alert=-1\$" 'client alert 47' 'client_certificates'
for mode in trailing truncated oddlist emptylist; do
    scenario "clientcert_refuses_a_malformed_request_$mode" "$mode" "$P256" "$P256K" 1 \
        "^conn 1 error=$REFUSED_MALFORMED alert=-1\$" 'client alert 50' 'client_certificates'
done
scenario clientcert_refuses_a_request_after_the_certificate aftercert "$P256" "$P256K" 1 \
    "^conn 1 error=$REFUSED_ORDER alert=-1\$" 'client alert 10' 'client_certificates'
scenario clientcert_refuses_two_requests twice "$P256" "$P256K" 1 \
    "^conn 1 error=$REFUSED_ORDER alert=-1\$" 'client alert 10' 'client_certificates'
scenario clientcert_hostile_request_without_identity_is_still_refused_as_such trailing none none 1 \
    "^conn 1 error=($REFUSED_MALFORMED|$REFUSED_ASKED) alert=-1\$" '1 client alert' 'client_certificates'

# --- openssl s_server --------------------------------------------------------------

if ! command -v openssl >/dev/null 2>&1 || [ -z "$SERVER_PIN" ]; then
    skip "openssl s_server checks: openssl is not installed or cannot read the fixtures"
else
    serve() { # serve <name> <s_server flags...>: sets SERVER_PORT, or returns 1.
        local name=$1
        shift
        SERVER_PORT=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
        openssl s_server -accept "127.0.0.1:$SERVER_PORT" -cert "$F/server.pem" -key "$F/server.key" -www "$@" \
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
    # live <name> <want> <chains> <keys> <connections> [server log regex]
    live() {
        local name=$1 want=$2 chains=$3 keys=$4 connections=$5 want_log=${6:--}
        local text rc
        text=$(timeout 90 "$CLIENT" "$SERVER_PORT" "$SERVER_PIN" "$chains" "$keys" "$connections" 2>"$WORK/client.$name.err" \
            | grep -v '^identity ')
        rc=${PIPESTATUS[0]}
        if [ "$rc" -ne 0 ]; then
            bad "$name" "client exited $rc" "output: $text" "$(tail -3 "$WORK/client.$name.err")"
        elif ! matches "$want" <<<"$text"; then
            bad "$name" "output did not match /$want/" "got: $text" "server: $(tail -4 "$WORK/s_server.$NAME.log" 2>/dev/null)"
        elif [ "$want_log" != - ] && ! grep -Eq "$want_log" "$WORK/s_server.$NAME.log"; then
            bad "$name" "the server's log did not match /$want_log/" "$(tail -8 "$WORK/s_server.$NAME.log")"
        else
            note "$name"
        fi
    }
    OK='^conn 1 ok resumed=false status=HTTP/1\.[01] 200 [Oo][Kk]$'
    verified() { echo "depth=0 CN *= *$1"; }

    NAME=require; serve require -tls1_3 -Verify 2 -verify_return_error -CAfile "$F/ca.pem" -num_tickets 1 && {
        live openssl_clientcert_p256 "$OK" "$P256" "$P256K" 1 "$(verified client-p256)"
        live openssl_clientcert_ed25519 "$OK" "$ED" "$EDK" 1 "$(verified client-ed25519)"
        live openssl_clientcert_chain_through_an_intermediate "$OK" "$F/deep.pem" "$F/deep.key" 1 "$(verified client-deep)"
        live openssl_clientcert_sec1_key "$OK" "$P256" "$F/p256sec1.key" 1
        live openssl_clientcert_missing_makes_the_client_stop \
            "^conn 1 error=$REFUSED_ASKED alert=-1\$" none none 1
        live openssl_clientcert_chain_without_its_intermediate_is_rejected_by_the_server \
            '^conn 1 error=.* alert=(48|42)$' "$F/deep_without_intermediate.pem" "$F/deep.key" 1
        live openssl_clientcert_from_an_untrusted_ca_is_rejected_by_the_server \
            '^conn 1 error=.* alert=48$' "$F/rogue.pem" "$F/rogue.key" 1
        live openssl_clientcert_expired_is_rejected_by_the_server \
            '^conn 1 error=.* alert=45$' "$F/expired.pem" "$F/expired.key" 1
        # A session made with a client certificate resumes for that certificate only.
        live openssl_clientcert_resumes_for_the_same_identity \
            '^conn 1 ok resumed=false .*\nconn 2 ok resumed=true .*\nconn 3 ok resumed=true ' \
            "$P256,$P256,$P256" "$P256K,$P256K,$P256K" 3
        live openssl_clientcert_session_is_not_shared_between_identities \
            '^conn 1 ok resumed=false .*\nconn 2 ok resumed=false .*\nconn 3 ok resumed=true .*\nconn 4 ok resumed=true ' \
            "$P256,$ED,$P256,$ED" "$P256K,$EDK,$P256K,$EDK" 4
        live openssl_clientcert_identity_less_connection_does_not_use_an_identity_session \
            '^conn 1 ok resumed=false .*\nconn 2 error='"$REFUSED_ASKED"' alert=-1\nconn 3 ok resumed=true ' \
            "$P256,none,$P256" "$P256K,none,$P256K" 3
    }
    NAME=optional; serve optional -tls1_3 -verify 2 -verify_return_error -CAfile "$F/ca.pem" -num_tickets 0 && {
        live openssl_clientcert_optional_request_with_identity "$OK" "$P256" "$P256K" 1 "$(verified client-p256)"
        live openssl_clientcert_optional_request_without_identity_still_stops \
            "^conn 1 error=$REFUSED_ASKED alert=-1\$" none none 1
    }
    NAME=norequest; serve norequest -tls1_3 -num_tickets 0 && {
        live openssl_clientcert_identity_is_not_sent_unasked "$OK" "$P256" "$P256K" 1
    }
    NAME=ed_only; serve ed_only -tls1_3 -Verify 2 -verify_return_error -CAfile "$F/ca.pem" -client_sigalgs ed25519 -num_tickets 0 && {
        live openssl_clientcert_ed25519_when_the_server_lists_only_ed25519 "$OK" "$ED" "$EDK" 1
        live openssl_clientcert_p256_key_when_the_server_lists_only_ed25519 \
            "^conn 1 error=$REFUSED_SCHEME alert=-1\$" "$P256" "$P256K" 1
    }
    NAME=p256_only; serve p256_only -tls1_3 -Verify 2 -verify_return_error -CAfile "$F/ca.pem" -client_sigalgs ECDSA+SHA256 -num_tickets 0 && {
        live openssl_clientcert_p256_when_the_server_lists_only_p256 "$OK" "$P256" "$P256K" 1
        live openssl_clientcert_ed25519_key_when_the_server_lists_only_p256 \
            "^conn 1 error=$REFUSED_SCHEME alert=-1\$" "$ED" "$EDK" 1
    }
    NAME=tls12; serve tls12 -tls1_2 -Verify 2 -verify_return_error -CAfile "$F/ca.pem" && {
        live openssl_clientcert_is_tls13_only_a_tls12_request_stops_the_client \
            "^conn 1 error=$REFUSED_ASKED alert=-1\$" "$P256" "$P256K" 1
    }
fi

echo
if [ $fail -eq 0 ]; then
    printf '\033[32mall %d clientcert checks passed\033[0m\n' "$pass"
else
    printf '\033[31m%d of %d clientcert checks FAILED\033[0m\n' "$fail" "$((pass + fail))"
fi
exit $fail
