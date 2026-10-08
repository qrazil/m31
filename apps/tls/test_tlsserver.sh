#!/usr/bin/env bash
# Every check for the TLS 1.3 server (`lib/tlsserver.m31`, `lib/signingkey.m31`,
# `lib/ecdsasign.m31`), against clients that are not this repository.
# Modelled on `apps/tls/test_tls.sh`.
#
#   bash apps/tls/test_tlsserver.sh
#
# Everything is disposable: a temporary directory holding a throwaway CA and
# localhost leaves (Ed25519 and P-256, made with openssl), servers on ephemeral
# loopback ports, readiness polled rather than slept for, cleanup on exit.
#
#   clients      openssl s_client, Python's ssl, curl (if installed), and the
#                m31 client in `lib/tls.m31` with Trust.CaFile -- each verifies
#                the chain, CertificateVerify and Finished for itself.
#   probe        apps/tls/tlsserver_probe.py: a minimal TLS 1.3 client written
#                from RFC 8446 that also misbehaves on request (garbage,
#                truncation, a wrong Finished, a TLS 1.2-only hello, ...), and
#                checks the alert it gets for each.
#   signers      m31 Ed25519 and ECDSA P-256 signatures against `cryptography`
#                (RFC 8032 and RFC 6979 vectors, then random keys and messages).
#   loading      PKCS#8 and SEC1 keys and chains, as PEM and as DER, and every way a
#                key file can be wrong, with a check that no error text echoes key
#                material.
#   deadline     a client dripping one octet at a time (and, for the m31 client, a
#                server doing so) is cut off at the whole-handshake deadline.
#
# SIGN_COUNT (default 2000) and ED_COUNT (default 1500) set the random cases.
# The compiler is `LANGC` (default `./target/debug/m31c`, built with `cargo build`).
set -uo pipefail
cd "$(dirname "$0")/../.."
. ./runtime/arch.sh

LANGC=${LANGC:-./target/debug/m31c}
SIGN_COUNT=${SIGN_COUNT:-2000}
ED_COUNT=${ED_COUNT:-1500}
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

# matches <regex>: does all of stdin match it?
matches() { perl -0777 -e 'local $/; my $text = <STDIN>; exit($text =~ /$ARGV[0]/ ? 0 : 1)' "$1"; }

# --- prerequisites ---------------------------------------------------------------

for tool in openssl python3 cc perl; do
    command -v "$tool" >/dev/null || { echo "test_tlsserver.sh needs $tool"; exit 1; }
done
if ! python3 -c 'import cryptography' 2>/dev/null; then
    echo "test_tlsserver.sh needs the python 'cryptography' package"
    exit 1
fi

# --- house style -----------------------------------------------------------------

if out=$(
    for f in lib/tlsserver.m31 lib/signingkey.m31 lib/ecdsasign.m31 apps/tls/t_tlsserver_*.m31 apps/tls/example_https_static.m31; do
        "$LANGC" fmt --check "$f" || echo "$f"
    done 2>&1
) && [ -z "$out" ]; then
    note "source is formatted"
else
    bad "source is not formatted (run: m31c fmt <file>)" "$out"
fi

for program in t_tlsserver_serve t_tlsserver_client t_tlsserver_config t_tlsserver_sign t_tlsserver_ed25519 example_https_static; do
    build "$program" || { echo; printf '\033[31m%d of %d tlsserver checks FAILED\033[0m\n' "$fail" "$((pass + fail))"; exit 1; }
done
SERVE=$WORK/t_tlsserver_serve
CLIENT=$WORK/t_tlsserver_client
CONFIG=$WORK/t_tlsserver_config

# --- the signers, in the background while the rest runs ----------------------------

signers() {
    python3 apps/tls/tlsserver_sign_oracle.py "$WORK/p256.cases" "$WORK/p256.expected" "$SIGN_COUNT" >"$WORK/p256.oracle" 2>&1 \
        && "$WORK/t_tlsserver_sign" "$WORK/p256.cases" >"$WORK/p256.got" 2>"$WORK/p256.err"
    echo $? >"$WORK/p256.rc"
}
ed_signers() {
    python3 apps/tls/tlsserver_ed25519_oracle.py "$WORK/ed.cases" "$WORK/ed.expected" "$ED_COUNT" >"$WORK/ed.oracle" 2>&1 \
        && "$WORK/t_tlsserver_ed25519" "$WORK/ed.cases" >"$WORK/ed.got" 2>"$WORK/ed.err"
    echo $? >"$WORK/ed.rc"
}
signers &
pids+=("$!")
ed_signers &
pids+=("$!")

# --- fixtures: a throwaway CA and localhost leaves ----------------------------------

FX=$WORK/fx
mkdir -p "$FX"
(
    cd "$FX" || exit 1
    openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out ca.key
    openssl req -x509 -new -key ca.key -sha256 -days 30 -subj /CN=tlsserver-test-ca \
        -addext basicConstraints=critical,CA:TRUE -addext keyUsage=critical,keyCertSign -out ca.pem
    leaf() { # leaf <file stem> <genpkey args...>  -- SAN from $SAN
        local stem=$1
        shift
        openssl genpkey "$@" -out "$stem.key"
        openssl req -new -key "$stem.key" -subj "/CN=$(first=${SAN%%,*}; echo "${first#DNS:}")" -out "$stem.csr"
        printf 'subjectAltName=%s\nbasicConstraints=CA:FALSE\nkeyUsage=digitalSignature\nextendedKeyUsage=serverAuth\n' "$SAN" >"$stem.ext"
        openssl x509 -req -in "$stem.csr" -CA ca.pem -CAkey ca.key -CAcreateserial -days 30 \
            -extfile "$stem.ext" -out "$stem.pem"
    }
    SAN=DNS:localhost,IP:127.0.0.1 leaf p -algorithm EC -pkeyopt ec_paramgen_curve:P-256
    SAN=DNS:localhost,IP:127.0.0.1 leaf ed -algorithm ed25519
    SAN=DNS:second.test leaf s -algorithm EC -pkeyopt ec_paramgen_curve:P-256
    openssl ec -in p.key -out p.sec1
    cat ed.pem ca.pem >ed.chain
    cp p.pem p.chain
    cp s.pem s.chain
    # The same material as DER: certificates, PKCS#8 and SEC1 keys.
    openssl x509 -in p.pem -outform DER -out p.der
    openssl x509 -in ed.pem -outform DER -out ed.der
    openssl x509 -in ca.pem -outform DER -out ca.der
    cat ed.der ca.der >ed.chain.der
    openssl pkcs8 -topk8 -nocrypt -in p.key -outform DER -out p.pk8.der
    openssl pkcs8 -topk8 -nocrypt -in ed.key -outform DER -out ed.pk8.der
    openssl ec -in p.key -outform DER -out p.sec1.der
) >"$WORK/fixtures.log" 2>&1 || { bad "fixtures" "$(tail -5 "$WORK/fixtures.log")"; exit 1; }
CA=$FX/ca.pem

# --- loading keys and chains --------------------------------------------------------

config_case() { # config_case <label> <chain> <key> <want regex over the output> <want rc>
    local label=$1 chain=$2 key=$3 want=$4 want_rc=$5
    TLSSERVER_CONFIG_API=${API:-files} "$CONFIG" "$chain" "$key" localhost LOCALHOST other.example 127.0.0.1 >"$WORK/config.out" 2>&1
    local rc=$?
    local text
    text=$(cat "$WORK/config.out")
    local leak=
    if [ -f "$key" ]; then
        local snippet
        if grep -q -a -e '-----BEGIN' "$key"; then
            snippet=$(grep -v -e '^-----' -e '^[A-Za-z-]*:' "$key" | head -2 | tr -d '\n')
            if [ ${#snippet} -ge 20 ] && grep -qF "${snippet:0:40}" <<<"$text"; then leak=yes; fi
        else
            # A DER key: its base64 and its hex must not appear either.
            snippet=$(base64 -w0 "$key" | cut -c9-48)
            if [ ${#snippet} -ge 20 ] && grep -qF "$snippet" <<<"$text"; then leak=yes; fi
            snippet=$(od -An -tx1 -v "$key" | tr -d ' \n' | cut -c17-56)
            if [ ${#snippet} -ge 20 ] && grep -qiF "$snippet" <<<"$text"; then leak=yes; fi
        fi
    fi
    if [ "$rc" -ne "$want_rc" ]; then
        bad "load: $label" "exit $rc, wanted $want_rc" "$text"
    elif ! matches "$want" <<<"$text"; then
        bad "load: $label" "output did not match /$want/" "$text"
    elif [ -n "$leak" ]; then
        bad "load: $label" "the output contains key material"
    else
        note "load: $label"
    fi
}

SERVES='^ok\nserves localhost yes\nserves LOCALHOST yes\nserves other.example no\nserves 127.0.0.1 yes\n$'
config_case "PKCS#8 P-256" "$FX/p.chain" "$FX/p.key" "$SERVES" 0
config_case "SEC1 EC P-256" "$FX/p.chain" "$FX/p.sec1" "$SERVES" 0
config_case "PKCS#8 Ed25519 with the CA after the leaf" "$FX/ed.chain" "$FX/ed.key" "$SERVES" 0
config_case "a key that is not the leaf's (Ed25519 for a P-256 leaf)" "$FX/p.chain" "$FX/ed.key" 'does not match' 1
config_case "a key that is not the leaf's (P-256 for an Ed25519 leaf)" "$FX/ed.chain" "$FX/p.key" 'does not match' 1
config_case "a key that is not the first certificate's" "$FX/ca.pem" "$FX/p.key" 'does not match' 1
config_case "a missing key file" "$FX/p.chain" "$FX/missing.key" 'could not be read' 1
config_case "a missing chain file" "$FX/missing.pem" "$FX/p.key" 'could not be read' 1
config_case "a certificate where the key belongs" "$FX/p.chain" "$FX/p.pem" 'no usable private key' 1
openssl genrsa -out "$FX/rsa.key" 2048 2>/dev/null
config_case "an RSA key (no RSA signing)" "$FX/p.chain" "$FX/rsa.key" 'cannot sign with' 1
openssl ecparam -name prime256v1 -genkey -noout 2>/dev/null | openssl pkcs8 -topk8 -v2 aes256 -passout pass:throwaway -out "$FX/enc.key" 2>/dev/null
config_case "an encrypted key" "$FX/p.chain" "$FX/enc.key" 'encrypted' 1
openssl ecparam -name secp384r1 -genkey -noout -out "$FX/p384.key" 2>/dev/null
config_case "a P-384 key" "$FX/p.chain" "$FX/p384.key" 'cannot sign with' 1
openssl ecparam -name prime256v1 -genkey -out "$FX/params.key" 2>/dev/null
config_case "an EC PARAMETERS block before a different key" "$FX/p.chain" "$FX/params.key" 'does not match' 1
: >"$FX/empty.key"
config_case "an empty key file" "$FX/p.chain" "$FX/empty.key" 'no usable private key' 1
printf -- '-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n' >"$FX/short.key"
config_case "a truncated PKCS#8 body" "$FX/p.chain" "$FX/short.key" 'error:' 1
printf -- '-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n' >"$FX/short.pem"
config_case "a truncated certificate" "$FX/short.pem" "$FX/p.key" 'error:' 1

# DER: the files form (told apart from PEM by content), then `load_config_der`.
config_case "DER: PKCS#8 P-256 key, DER certificate" "$FX/p.der" "$FX/p.pk8.der" "$SERVES" 0
config_case "DER: SEC1 EC P-256 key" "$FX/p.der" "$FX/p.sec1.der" "$SERVES" 0
config_case "DER: PKCS#8 Ed25519 key, two concatenated DER certificates" "$FX/ed.chain.der" "$FX/ed.pk8.der" "$SERVES" 0
config_case "DER chain with a PEM key" "$FX/ed.chain.der" "$FX/ed.key" "$SERVES" 0
config_case "PEM chain with a DER key" "$FX/p.chain" "$FX/p.pk8.der" "$SERVES" 0
config_case "DER: Ed25519 key for a P-256 leaf" "$FX/p.der" "$FX/ed.pk8.der" 'does not match' 1
config_case "DER: P-256 key for an Ed25519 leaf" "$FX/ed.der" "$FX/p.pk8.der" 'does not match' 1
config_case "DER: SEC1 key for an Ed25519 leaf" "$FX/ed.der" "$FX/p.sec1.der" 'does not match' 1
openssl pkcs8 -topk8 -v2 aes256 -passout pass:throwaway -in "$FX/p.key" -outform DER -out "$FX/enc.pk8.der" 2>/dev/null
config_case "DER: an encrypted PKCS#8 key" "$FX/p.der" "$FX/enc.pk8.der" 'encrypted' 1
openssl rsa -in "$FX/rsa.key" -outform DER -out "$FX/rsa.pkcs1.der" 2>/dev/null
openssl pkcs8 -topk8 -nocrypt -in "$FX/rsa.key" -outform DER -out "$FX/rsa.pk8.der" 2>/dev/null
config_case "DER: an RSA key, PKCS#1 (no RSA signing)" "$FX/p.der" "$FX/rsa.pkcs1.der" 'cannot sign with' 1
config_case "DER: an RSA key, PKCS#8 (no RSA signing)" "$FX/p.der" "$FX/rsa.pk8.der" 'cannot sign with' 1
openssl pkcs8 -topk8 -nocrypt -in "$FX/p384.key" -outform DER -out "$FX/p384.pk8.der" 2>/dev/null
config_case "DER: a P-384 key" "$FX/p.der" "$FX/p384.pk8.der" 'cannot sign with' 1
head -c 40 "$FX/p.pk8.der" >"$FX/trunc.pk8.der"
config_case "DER: a truncated PKCS#8 key" "$FX/p.der" "$FX/trunc.pk8.der" 'no usable private key' 1
printf 'this is not a key at all, only a sentence of text' >"$FX/garbage.der"
config_case "DER: garbage where the key belongs" "$FX/p.der" "$FX/garbage.der" 'no usable private key' 1
config_case "DER: a certificate where the key belongs" "$FX/p.der" "$FX/p.der" 'no usable private key' 1
head -c 100 "$FX/p.der" >"$FX/trunc.der"
config_case "DER: a truncated certificate" "$FX/trunc.der" "$FX/p.pk8.der" 'could not be parsed' 1
config_case "DER: garbage where the chain belongs" "$FX/garbage.der" "$FX/p.pk8.der" 'could not be parsed' 1
: >"$FX/empty.der"
config_case "DER: an empty chain file" "$FX/empty.der" "$FX/p.pk8.der" 'could not be parsed' 1
API=der config_case "load_config_der: P-256 PKCS#8" "$FX/p.der" "$FX/p.pk8.der" "$SERVES" 0
API=der config_case "load_config_der: P-256 SEC1" "$FX/p.der" "$FX/p.sec1.der" "$SERVES" 0
API=der config_case "load_config_der: Ed25519 PKCS#8" "$FX/ed.der" "$FX/ed.pk8.der" "$SERVES" 0
API=der config_case "load_config_der: a mismatched key" "$FX/p.der" "$FX/ed.pk8.der" 'does not match' 1
API=der config_case "load_config_der: an encrypted key" "$FX/p.der" "$FX/enc.pk8.der" 'encrypted' 1
API=der config_case "load_config_der: an RSA key" "$FX/p.der" "$FX/rsa.pk8.der" 'cannot sign with' 1
API=der config_case "load_config_der: PEM text as the key" "$FX/p.der" "$FX/p.key" 'no usable private key' 1

# --- servers -------------------------------------------------------------------------

SERVER_PORT=
SERVER_LOG=
SERVER_PID=
# start_server <name> <chain> <key> <mode> <timeout ms> <protocols> [chain2 key2]
start_server() {
    local name=$1
    shift
    SERVER_LOG=$WORK/server.$name.log
    "$SERVE" "$@" >"$SERVER_LOG" 2>"$WORK/server.$name.err" </dev/null &
    SERVER_PID=$!
    pids+=("$SERVER_PID")
    if ! await_line "$SERVER_LOG" '^READY ' "$SERVER_PID"; then
        bad "server $name did not start" "$(tail -5 "$WORK/server.$name.err")"
        return 1
    fi
    SERVER_PORT=$(awk '/^READY /{print $2; exit}' "$SERVER_LOG")
}
stop_server() {
    kill "$SERVER_PID" 2>/dev/null
    wait "$SERVER_PID" 2>/dev/null
}
server_alive() { kill -0 "$SERVER_PID" 2>/dev/null; }

# py_get <port> <server name> <expected alpn or -> [alpn list, comma separated or -]
# Python's ssl, verifying the chain against the test CA and the name against the SAN.
py_get() {
    python3 - "$@" "$CA" <<'EOF'
import socket, ssl, sys
port, name, want_alpn, offered, ca = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5]
context = ssl.create_default_context(cafile=ca)
context.minimum_version = ssl.TLSVersion.TLSv1_3
context.set_ciphers("DEFAULT")
if offered != "-":
    context.set_alpn_protocols(offered.split(","))
with socket.create_connection(("127.0.0.1", port), timeout=15) as raw:
    with context.wrap_socket(raw, server_hostname=name) as tls:
        tls.sendall(b"GET / HTTP/1.1\r\nHost: " + name.encode() + b"\r\nConnection: close\r\n\r\n")
        data = b""
        while True:
            chunk = tls.recv(65536)
            if not chunk:
                break
            data += chunk
        head, _, body = data.partition(b"\r\n\r\n")
        subject = dict(item[0] for item in tls.getpeercert()["subject"])
        alpn = tls.selected_alpn_protocol() or "-"
        print("version=%s cipher=%s alpn=%s cn=%s status=%s size=%d" % (
            tls.version(), tls.cipher()[0], alpn, subject["commonName"], head.split(b"\r\n")[0].decode(), len(body)))
        if want_alpn != "any" and alpn != want_alpn:
            sys.exit("ALPN %s, wanted %s" % (alpn, want_alpn))
EOF
}

# --- server A: P-256 leaf, small page, ALPN http/1.1 ----------------------------------

if start_server p256 "$FX/p.chain" "$FX/p.key" static 3000 http/1.1; then
    PORT=$SERVER_PORT

    out=$(printf 'GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n' \
        | timeout 30 openssl s_client -tls1_3 -connect "127.0.0.1:$PORT" -servername localhost -CAfile "$CA" \
            -verify_return_error -verify_hostname localhost -ign_eof -alpn http/1.1 -ciphersuites TLS_CHACHA20_POLY1305_SHA256 2>&1)
    if matches 'Verification: OK|Verify return code: 0 \(ok\)' <<<"$out" \
        && matches 'TLSv1\.3' <<<"$out" && matches 'TLS_CHACHA20_POLY1305_SHA256' <<<"$out" \
        && matches 'ALPN protocol: http/1\.1' <<<"$out" && matches 'hello over https' <<<"$out" \
        && matches '[Ss]ignature type: ECDSA' <<<"$out"; then
        note "openssl s_client -tls1_3: chain, name, ALPN and the page, ECDSA P-256"
    else
        bad "openssl s_client against the P-256 server" "$(printf '%s' "$out" | head -30)"
    fi

    if out=$(py_get "$PORT" localhost http/1.1 http/1.1,h2 2>&1) && matches 'version=TLSv1\.3 cipher=TLS_CHACHA20_POLY1305_SHA256 alpn=http/1\.1 cn=localhost status=HTTP/1\.1 200 OK size=17' <<<"$out"; then
        note "python ssl: verified chain and name, ChaCha20-Poly1305, ALPN, the page"
    else
        bad "python ssl against the P-256 server" "$out"
    fi

    if command -v curl >/dev/null; then
        out=$(timeout 30 curl -sS --cacert "$CA" --resolve "localhost:$PORT:127.0.0.1" --http1.1 \
            -o "$WORK/curl.body" -w '%{http_code} %{size_download} %{ssl_verify_result}' "https://localhost:$PORT/" 2>&1)
        if [ "$out" = "200 17 0" ] && [ "$(cat "$WORK/curl.body")" = "hello over https" ]; then
            note "curl --cacert: 200, 17 octets, certificate verified"
        else
            bad "curl against the P-256 server" "$out"
        fi
    else
        skip "curl is not installed"
    fi

    out=$(timeout 60 "$CLIENT" localhost 127.0.0.1 "$PORT" "$CA" 2>&1)
    if [ "$out" = "$(printf 'status HTTP/1.1 200 OK\nsize 17')" ]; then
        note "m31 client (lib/tls.m31, Trust.CaFile) against the m31 server"
    else
        bad "m31 client against the P-256 server" "$out"
    fi
    out=$(timeout 60 "$CLIENT" wrong.example 127.0.0.1 "$PORT" "$CA" 2>&1)
    if [ $? -ne 0 ] && matches '^error:' <<<"$out"; then
        note "m31 client: a name the certificate does not cover is refused"
    else
        bad "m31 client with the wrong name" "$out"
    fi

    # What a server must refuse, each with a clean error and no hang.
    out=$(printf '' | timeout 30 openssl s_client -tls1_2 -connect "127.0.0.1:$PORT" -servername localhost -CAfile "$CA" 2>&1)
    if matches 'alert protocol version|protocol version' <<<"$out"; then
        note "openssl -tls1_2: protocol_version, not a handshake"
    else
        bad "a TLS 1.2-only client" "$(printf '%s' "$out" | head -12)"
    fi
    out=$(printf '' | timeout 30 openssl s_client -tls1_3 -ciphersuites TLS_AES_128_GCM_SHA256 -connect "127.0.0.1:$PORT" -servername localhost -CAfile "$CA" 2>&1)
    if matches 'handshake failure' <<<"$out"; then
        note "openssl with only TLS_AES_128_GCM_SHA256: handshake_failure"
    else
        bad "an AES-only client" "$(printf '%s' "$out" | head -12)"
    fi
    out=$(printf '' | timeout 30 openssl s_client -tls1_3 -connect "127.0.0.1:$PORT" -servername other.example -CAfile "$CA" 2>&1)
    if matches 'unrecognized name|alert unrecognized' <<<"$out"; then
        note "openssl with an unknown SNI: unrecognized_name"
    else
        bad "an unknown server name" "$(printf '%s' "$out" | head -12)"
    fi
    out=$(printf '' | timeout 30 openssl s_client -tls1_3 -connect "127.0.0.1:$PORT" -servername localhost -alpn spdy/3 -CAfile "$CA" 2>&1)
    if matches 'no application protocol' <<<"$out"; then
        note "openssl offering only spdy/3: no_application_protocol"
    else
        bad "an ALPN list with no common protocol" "$(printf '%s' "$out" | head -12)"
    fi
    if out=$(python3 - "$PORT" "$CA" 2>&1 <<'EOF'
import socket, ssl, sys
context = ssl.create_default_context(cafile=sys.argv[2])
context.maximum_version = ssl.TLSVersion.TLSv1_2
try:
    with socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=15) as raw:
        context.wrap_socket(raw, server_hostname="localhost")
    sys.exit("a TLS 1.2 handshake succeeded")
except ssl.SSLError as error:
    print("refused: %s" % error)
EOF
    ) && matches '^refused:' <<<"$out"; then
        note "python ssl capped at TLS 1.2: refused"
    else
        bad "python ssl capped at TLS 1.2" "$out"
    fi

    # Many sequential connections, all of them fully verified.
    if out=$(python3 - "$PORT" "$CA" 2>&1 <<'EOF'
import socket, ssl, sys
context = ssl.create_default_context(cafile=sys.argv[2])
context.minimum_version = ssl.TLSVersion.TLSv1_3
for index in range(60):
    with socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=15) as raw:
        with context.wrap_socket(raw, server_hostname="localhost") as tls:
            tls.sendall(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            data = b""
            while True:
                chunk = tls.recv(65536)
                if not chunk:
                    break
                data += chunk
            assert data.endswith(b"hello over https\n"), (index, data)
print("60 connections")
EOF
    ); then
        note "60 sequential verified connections ($out)"
    else
        bad "sequential connections" "$out"
    fi

    # The protocol-level client: every case, including the ones a real client never sends.
    probe=$(timeout 120 python3 apps/tls/tlsserver_probe.py "$PORT" "$CA" \
        full full_no_sni alpn_http hrr hrr_twice psk_ignored \
        bad_finished cert_as_finished no_extensions garbage truncated oversized stalled \
        tls12_only aes_only no_sigalg wrong_sni alpn_mismatch no_x25519 dup_extension full 2>&1)
    probe_total=$(grep -c -E '^(ok|FAIL) ' <<<"$probe")
    probe_bad=$(grep -c '^FAIL ' <<<"$probe")
    if [ "$probe_total" -eq 21 ] && [ "$probe_bad" -eq 0 ]; then
        note "probe: $probe_total protocol cases (HRR, bad Finished, garbage, truncation, TLS 1.2, AES-only, SNI, ALPN, ...)"
    else
        bad "probe cases" "$(grep -v '^ok ' <<<"$probe" | head -12)" "ran $probe_total of 21"
    fi
    if server_alive; then
        note "the server is still serving after every negative case"
    else
        bad "the server died during the negative cases" "$(tail -5 "$WORK/server.p256.err")"
    fi
    stop_server
fi

# --- server B: Ed25519 leaf, a 1 MiB response -----------------------------------------

if start_server ed25519 "$FX/ed.chain" "$FX/ed.key" big 3000 http/1.1; then
    PORT=$SERVER_PORT

    out=$(printf 'GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n' \
        | timeout 60 openssl s_client -tls1_3 -connect "127.0.0.1:$PORT" -servername localhost -CAfile "$CA" \
            -verify_return_error -verify_hostname localhost 2>&1 | tr -d '\000')
    if matches 'Verification: OK|Verify return code: 0 \(ok\)' <<<"$out" && matches '[Ss]ignature type: ED25519|ed25519' <<<"$out"; then
        note "openssl s_client: Ed25519 CertificateVerify and the chain (CA sent after the leaf)"
    else
        bad "openssl s_client against the Ed25519 server" "$(printf '%s' "$out" | head -20)"
    fi

    if out=$(py_get "$PORT" localhost any - 2>&1) && matches 'size=1048576' <<<"$out"; then
        note "python ssl: a 1 MiB body over Ed25519 ($out)"
    else
        bad "python ssl 1 MiB" "$out"
    fi
    if command -v curl >/dev/null; then
        out=$(timeout 60 curl -sS --cacert "$CA" --resolve "localhost:$PORT:127.0.0.1" --http1.1 \
            -o "$WORK/curl.big" -w '%{http_code} %{size_download} %{ssl_verify_result}' "https://localhost:$PORT/" 2>&1)
        if [ "$out" = "200 1048576 0" ] && python3 - "$WORK/curl.big" <<'EOF'
import sys
body = open(sys.argv[1], "rb").read()
sys.exit(0 if all(body[i] == i % 251 for i in range(len(body))) else 1)
EOF
        then
            note "curl: a 1 MiB body, every octet checked"
        else
            bad "curl 1 MiB" "$out"
        fi
    fi

    # The m31 client offers Ed25519 (0x0807): CertificateVerify and the chain's Ed25519 leaf both check.
    out=$(timeout 120 "$CLIENT" localhost 127.0.0.1 "$PORT" "$CA" 2>&1)
    if [ "$out" = "$(printf 'status HTTP/1.1 200 OK\nsize 1048576')" ]; then
        note "m31 client against an Ed25519 certificate: Ed25519 CertificateVerify, 1 MiB"
    else
        bad "m31 client against the Ed25519 server" "$out"
    fi
    # An IP-literal host is matched against the iPAddress SAN only.
    out=$(timeout 120 "$CLIENT" 127.0.0.1 127.0.0.1 "$PORT" "$CA" 2>&1)
    if [ "$out" = "$(printf 'status HTTP/1.1 200 OK\nsize 1048576')" ]; then
        note "m31 client dialling 127.0.0.1 by name: the certificate's iPAddress SAN matches"
    else
        bad "m31 client with an IP-literal host" "$out"
    fi
    out=$(timeout 60 "$CLIENT" 127.0.0.2 127.0.0.1 "$PORT" "$CA" 2>&1)
    if [ $? -ne 0 ] && matches '^error:' <<<"$out"; then
        note "m31 client with an IP host the certificate does not list: refused"
    else
        bad "m31 client with the wrong IP host" "$out"
    fi

    probe=$(timeout 120 python3 apps/tls/tlsserver_probe.py "$PORT" "$CA" full full_big hrr no_sigalg 2>&1)
    if [ "$(grep -c '^ok ' <<<"$probe")" -eq 4 ] && matches 'ok full scheme=0x0807' <<<"$probe"; then
        note "probe against Ed25519: scheme 0x0807 verified, 1 MiB, HRR, no common scheme"
    else
        bad "probe against the Ed25519 server" "$probe"
    fi
    server_alive || bad "the Ed25519 server died" "$(tail -3 "$WORK/server.ed25519.err")"
    stop_server
fi

# --- server C: two certificates, chosen by SNI ----------------------------------------

if start_server sni "$FX/p.chain" "$FX/p.key" static 3000 - "$FX/s.chain" "$FX/s.key"; then
    PORT=$SERVER_PORT
    if out=$(py_get "$PORT" second.test any - 2>&1) && matches 'cn=second\.test' <<<"$out"; then
        note "SNI second.test selects the second certificate"
    else
        bad "SNI selecting the second certificate" "$out"
    fi
    if out=$(py_get "$PORT" localhost any - 2>&1) && matches 'cn=localhost' <<<"$out" && matches 'alpn=-' <<<"$out"; then
        note "SNI localhost selects the first certificate; no ALPN configured, none negotiated"
    else
        bad "SNI selecting the first certificate" "$out"
    fi
    out=$(printf '' | timeout 30 openssl s_client -tls1_3 -connect "127.0.0.1:$PORT" -noservername -CAfile "$CA" 2>&1)
    if matches 'CN ?= ?localhost' <<<"$out"; then
        note "no SNI at all: the first certificate"
    else
        bad "no SNI" "$(printf '%s' "$out" | head -12)"
    fi
    out=$(printf '' | timeout 30 openssl s_client -tls1_3 -connect "127.0.0.1:$PORT" -servername nowhere.test -CAfile "$CA" 2>&1)
    if matches 'unrecognized name' <<<"$out"; then
        note "an SNI no certificate covers: unrecognized_name"
    else
        bad "an SNI no certificate covers" "$(printf '%s' "$out" | head -12)"
    fi
    stop_server
fi

# --- the whole-handshake deadline --------------------------------------------------------
#
# The per-read timeout is 3 s here and the deadline 2 s. A client that sends one octet
# every 0.4 s never lets a read time out, and would keep a handshake open for minutes
# (it has 300 octets to send); only the total bound cuts it off.

if TLSSERVER_DEADLINE_MS=2000 start_server drip "$FX/p.chain" "$FX/p.key" static 3000 http/1.1; then
    PORT=$SERVER_PORT
    if out=$(python3 - "$PORT" 2>&1 <<'EOF'
import socket, sys, time
start = time.monotonic()
sock = socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=5)
sock.settimeout(0.4)
sock.sendall(bytes([0x16, 0x03, 0x01, 0x01, 0x2c]))      # a record header promising 300 octets
sent = 0
ended = None
while time.monotonic() - start < 12 and sent < 300:
    try:
        sock.sendall(b"\x01")
        sent += 1
    except OSError:
        ended = "send failed"
        break
    try:
        data = sock.recv(100)
        ended = "closed" if data == b"" else "data"
        break
    except socket.timeout:
        pass
    except OSError:
        ended = "reset"
        break
elapsed = time.monotonic() - start
print("ended=%s elapsed=%.1f octets=%d" % (ended, elapsed, sent))
if ended is None:
    sys.exit("the server was still waiting after %.1f s" % elapsed)
if not 1.5 <= elapsed <= 5.0:
    sys.exit("cut off at %.1f s, wanted about 2 s" % elapsed)
EOF
    ); then
        note "a client dripping one octet per 0.4 s is cut off at the 2 s deadline ($out)"
    else
        bad "the handshake deadline against a dripping client" "$out"
    fi
    if matches 'conn 1 error' <"$SERVER_LOG"; then
        note "the cut-off handshake is reported as an error"
    else
        bad "the dripped handshake was not reported" "$(cat "$SERVER_LOG")"
    fi
    if out=$(py_get "$PORT" localhost http/1.1 http/1.1 2>&1) && matches 'status=HTTP/1\.1 200 OK' <<<"$out"; then
        note "the server still serves a normal client after the cut-off"
    else
        bad "serving after the deadline fired" "$out"
    fi
    out=$(timeout 60 "$CLIENT" localhost 127.0.0.1 "$PORT" "$CA" 20000 5000 2>&1)
    if [ "$out" = "$(printf 'status HTTP/1.1 200 OK\nsize 17')" ]; then
        note "m31 client under connect_over_deadline: a normal handshake completes and the connection works after release"
    else
        bad "m31 client with a deadline against the m31 server" "$out"
    fi
    server_alive || bad "the deadline server died" "$(tail -3 "$WORK/server.drip.err")"
    stop_server
fi

# The client side: a server that dribbles a record one octet at a time.
if out=$(python3 - "$CLIENT" "$CA" 2>&1 <<'EOF'
import socket, subprocess, sys, threading, time
listener = socket.socket()
listener.bind(("127.0.0.1", 0))
listener.listen(1)
port = listener.getsockname()[1]
stop = threading.Event()
def serve():
    conn, _ = listener.accept()
    conn.settimeout(0.3)
    try:
        conn.recv(65536)
    except OSError:
        pass
    try:
        conn.sendall(bytes([0x16, 0x03, 0x03, 0x01, 0x2c]))
        for _ in range(300):
            if stop.is_set():
                break
            time.sleep(0.4)
            conn.sendall(b"\x02")
    except OSError:
        pass
    conn.close()
thread = threading.Thread(target=serve, daemon=True)
thread.start()
start = time.monotonic()
done = subprocess.run([sys.argv[1], "localhost", "127.0.0.1", str(port), sys.argv[2], "2000", "3000"],
                      capture_output=True, text=True, timeout=30)
elapsed = time.monotonic() - start
stop.set()
print("exit=%d elapsed=%.1f out=%s" % (done.returncode, elapsed, done.stdout.strip()))
if done.returncode == 0 or not done.stdout.startswith("error:"):
    sys.exit("the client did not fail")
if not 1.5 <= elapsed <= 6.0:
    sys.exit("cut off at %.1f s, wanted about 2 s" % elapsed)
EOF
); then
    note "m31 client: connect_over_deadline cuts off a server dripping a record ($out)"
else
    bad "the client handshake deadline" "$out"
fi

# --- the echo mode: reads, writes and a clean close ------------------------------------

if start_server echo "$FX/p.chain" "$FX/p.key" echo 3000 -; then
    if out=$(python3 - "$SERVER_PORT" "$CA" 2>&1 <<'EOF'
import socket, ssl, sys
context = ssl.create_default_context(cafile=sys.argv[2])
with socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=15) as raw:
    with context.wrap_socket(raw, server_hostname="localhost") as tls:
        reader = tls.makefile("rb")
        for text in [b"one", b"two", b"x" * 20000]:
            tls.sendall(text + b"\n")
            line = reader.readline()
            assert line == b"echo: " + text + b"\n", line[:50]
        tls.unwrap()
print("3 lines, close_notify exchanged")
EOF
    ); then
        note "echo: lines of 3 and 20000 octets round-trip, close_notify both ways ($out)"
    else
        bad "echo mode" "$out"
    fi
    stop_server
fi

# --- the example program ---------------------------------------------------------------

EXAMPLE_LOG=$WORK/example.log
"$WORK/example_https_static" "$FX/p.chain" "$FX/p.key" 0 >"$EXAMPLE_LOG" 2>"$WORK/example.err" </dev/null &
EXAMPLE_PID=$!
pids+=("$EXAMPLE_PID")
if await_line "$EXAMPLE_LOG" '^listening on ' "$EXAMPLE_PID"; then
    port=$(awk '/^listening on /{print $3; exit}' "$EXAMPLE_LOG")
    if out=$(py_get "$port" localhost any - 2>&1) && matches 'status=HTTP/1\.1 200 OK size=17' <<<"$out"; then
        note "example_https_static serves its page over https"
    else
        bad "example_https_static" "$out" "$(tail -3 "$WORK/example.err")"
    fi
else
    bad "example_https_static did not start" "$(tail -3 "$WORK/example.err")"
fi
kill "$EXAMPLE_PID" 2>/dev/null
wait "$EXAMPLE_PID" 2>/dev/null

# --- the signers' verdicts ---------------------------------------------------------------

wait_for_file() {
    local tries=0
    until [ -f "$1" ]; do
        tries=$((tries + 1))
        [ $tries -le 2400 ] || return 1
        python3 -c 'import time; time.sleep(0.1)'
    done
}
if wait_for_file "$WORK/p256.rc" && [ "$(cat "$WORK/p256.rc")" = 0 ] && diff -q "$WORK/p256.got" "$WORK/p256.expected" >/dev/null; then
    note "ECDSA P-256 signer: RFC 6979 A.2.5 vectors and $(wc -l <"$WORK/p256.expected") cases byte-identical to the reference; OpenSSL verifies every signature"
else
    bad "ECDSA P-256 signer" "$(tail -3 "$WORK/p256.oracle")" "$(diff "$WORK/p256.got" "$WORK/p256.expected" 2>&1 | head -4)" "$(tail -3 "$WORK/p256.err")"
fi
if wait_for_file "$WORK/ed.rc" && [ "$(cat "$WORK/ed.rc")" = 0 ] && diff -q "$WORK/ed.got" "$WORK/ed.expected" >/dev/null; then
    note "Ed25519 signer: RFC 8032 vectors and $(wc -l <"$WORK/ed.expected") cases byte-identical to cryptography"
else
    bad "Ed25519 signer" "$(tail -3 "$WORK/ed.oracle")" "$(diff "$WORK/ed.got" "$WORK/ed.expected" 2>&1 | head -4)" "$(tail -3 "$WORK/ed.err")"
fi

echo
if [ $fail -eq 0 ]; then
    printf '\033[32mall %d tlsserver checks passed\033[0m\n' "$pass"
else
    printf '\033[31m%d of %d tlsserver checks FAILED\033[0m\n' "$fail" "$((pass + fail))"
fi
exit $fail
