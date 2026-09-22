#!/usr/bin/env bash
# Run runtime/sys_test.c against every sys-layer backend this machine can
# build and run. Exits non-zero if any build or any check fails.
#
#   libc   gcc and clang, -O0 and -O2          always
#   raw    gcc and clang, -O0 and -O2          on x86-64, aarch64, riscv64 Linux
#   raw, no libc at all, static                 same, natively
#   raw, no libc, aarch64 and riscv64           when clang, an lld and qemu-user
#                                               are all present; skipped, and
#                                               said so, otherwise
#
# The freestanding builds are the ones that matter most: with no C library
# linked, a raw backend that quietly called one would not link.
set -uo pipefail
cd "$(dirname "$0")/.."

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
fail=0

# check <label> <binary> [runner...]
check() {
    local label=$1 bin=$2
    shift 2
    local out
    if out=$("$@" "$bin" 2>&1); then
        printf '%-44s %s\n' "$label" "$(tail -1 <<<"$out")"
    else
        printf '%-44s FAILED\n' "$label"
        sed 's/^/    /' <<<"$out"
        fail=1
    fi
}

# build <label> <cc> <args...> -- compile, and a warning is a failure.
build() {
    local label=$1 out=$2
    shift 2
    if ! "$@" -o "$out" 2>"$WORK/cc.err" || [ -s "$WORK/cc.err" ]; then
        printf '%-44s FAILED to build\n' "$label"
        sed 's/^/    /' "$WORK/cc.err" | head -10
        fail=1
        return 1
    fi
}

arch=$(uname -m)
raw_native=0
if [ "$(uname -s)" = Linux ]; then
    case $arch in x86_64|aarch64|riscv64) raw_native=1 ;; esac
fi

for cc in gcc clang; do
    command -v "$cc" >/dev/null || continue
    for opt in -O0 -O2; do
        l="libc $cc $opt"
        build "$l" "$WORK/t" "$cc" "$opt" -Wall -Wextra -I runtime runtime/sys_test.c &&
            check "$l" "$WORK/t"
        [ $raw_native -eq 1 ] || continue
        l="raw  $cc $opt"
        build "$l" "$WORK/t" "$cc" "$opt" -Wall -Wextra -DRT_SYS_RAW -I runtime runtime/sys_test.c &&
            check "$l" "$WORK/t"
    done
done

# No C library at all. -fno-stack-protector because the protector's guard
# lives in the C library's thread block, which does not exist here.
FREE=(-O2 -Wall -Wextra -DRT_SYS_RAW -DSYS_TEST_FREESTANDING -ffreestanding
      -fno-stack-protector -fno-builtin -nostdlib -static -I runtime runtime/sys_test.c)
if [ $raw_native -eq 1 ]; then
    l="raw  gcc -nostdlib -static ($arch)"
    build "$l" "$WORK/f" gcc "${FREE[@]}" && check "$l" "$WORK/f"
fi

# Cross: clang can target any architecture, but linking needs an lld. The
# Rust toolchain this repository already requires ships one as rust-lld.
lld=""
if command -v ld.lld >/dev/null; then
    lld=$(command -v ld.lld)
elif command -v rustc >/dev/null; then
    host=$(rustc -vV | sed -n 's/^host: //p')
    cand="$(rustc --print sysroot)/lib/rustlib/$host/bin/gcc-ld/ld.lld"
    [ -x "$cand" ] && lld=$cand
fi

for target in aarch64 riscv64; do
    [ "$target" = "$arch" ] && continue
    qemu=""
    for q in "qemu-$target" "qemu-$target-static"; do
        command -v "$q" >/dev/null && { qemu=$q; break; }
    done
    missing=""
    command -v clang >/dev/null || missing="clang"
    [ -n "$lld" ] || missing="$missing an lld"
    [ -n "$qemu" ] || missing="$missing qemu-$target"
    l="raw  clang -nostdlib -static ($target)"
    if [ -n "$missing" ]; then
        printf '%-44s skipped (no%s)\n' "$l" "$missing"
        continue
    fi
    # -mno-relax: without it lld may relax addresses against a global
    # pointer that nobody set up, because there is no C runtime to do it.
    extra=()
    [ "$target" = riscv64 ] && extra=(-mno-relax)
    build "$l" "$WORK/x" clang --target="$target-linux-gnu" "${extra[@]}" \
        --ld-path="$lld" "${FREE[@]}" && check "$l" "$WORK/x" "$qemu"
done

exit $fail
