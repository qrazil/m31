#!/bin/sh
# Alpine / musl CI body: run INSIDE an alpine container with the repo mounted
# as the working directory (see .github/workflows/alpine.yml and release.yml).
#
#   sh scripts/ci-alpine.sh gates     every gate (./gates.sh) on musl
#   sh scripts/ci-alpine.sh bundle    static release compiler + SDK tarball
#                                     -> m31-linux-<arch>-musl.tar.gz
#
# Rust is Alpine's own (its host target is *-unknown-linux-musl). The release
# compiler is linked fully static (+crt-static): one file that runs on any
# Linux of that CPU, glibc or musl, with no libgcc_s/libc.so to match.
set -eu
cd "$(dirname "$0")/.."

# bash + GNU tools: the gates are bash scripts written against GNU
# coreutils/sed/grep/diff; busybox applets differ in flags. perl:
# runtime/tools/with_timeout.sh. go: the corpus's Go twins. python3: the
# proc/fs tests' oracle.
apk add --no-cache bash coreutils diffutils findutils grep sed gawk perl \
    python3 go git tar gzip file binutils \
    gcc clang musl-dev linux-headers compiler-rt \
    rust cargo rust-clippy rustfmt >/dev/null
git config --global --add safe.directory "*"

arch=$(uname -m)
case "$arch" in arm64) arch=aarch64 ;; esac

case "${1:-}" in
gates)
    exec bash ./gates.sh
    ;;
bundle)
    . ./config.sh
    RUSTFLAGS="-C target-feature=+crt-static" cargo build --release
    bin="target/release/$LANG_BIN"
    # A static executable has no interpreter: `file` says "static-pie" or
    # "statically linked"; ldd on one prints "not a dynamic executable" (or
    # musl's ldd lists only itself).
    file "$bin"
    if readelf -l "$bin" | grep -q INTERP; then
        echo "bundle: $bin is dynamically linked" >&2
        exit 1
    fi
    OUT="dist/$LANG_NAME-linux-$arch-musl"
    rm -rf dist
    mkdir -p "$OUT"
    cp "$bin" "$OUT/$LANG_BIN"
    cp config.sh "$OUT/config.sh"
    cp -r runtime "$OUT/runtime"
    rm -rf "$OUT/runtime/tests"
    # Smoke: the shipped SDK, not the checkout, builds and runs a program.
    # (build.sh hardcodes target/debug/$LANG_BIN, so drive cc directly.)
    t=$(mktemp -d)
    printf 'print("musl ok");\n' >"$t/hello.$LANG_EXT"
    "$OUT/$LANG_BIN" --emit-c "$t/hello.$LANG_EXT" -o "$t/hello.c"
    . "$OUT/runtime/arch.sh"
    (cd "$OUT" && cc -O2 -pthread -I runtime -o "$t/hello" "$t/hello.c" \
        runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM")
    [ "$("$t/hello")" = "musl ok" ] || { echo "bundle: smoke program wrong" >&2; exit 1; }
    cat >"$OUT/NOTES.txt" <<NOTES
$LANG_NAME - Linux $arch (musl / Alpine) release build
Built from commit: ${GITHUB_SHA:-unknown}
Tag: ${GITHUB_REF_NAME:-unknown}

$LANG_BIN                the compiler, statically linked (runs on glibc and musl alike)
config.sh, runtime/      the runtime SDK, identical to the glibc tarballs; programs
                         are compiled by the system cc, so install gcc/clang and
                         musl-dev on Alpine (apk add gcc musl-dev linux-headers).
                         Point M31_ROOT at this directory.
NOTES
    tar -C "$OUT" -czf "$LANG_NAME-linux-$arch-musl.tar.gz" .
    ls -l "$LANG_NAME-linux-$arch-musl.tar.gz"
    ;;
*)
    echo "usage: ci-alpine.sh gates|bundle" >&2
    exit 2
    ;;
esac
