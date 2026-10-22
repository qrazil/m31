#!/usr/bin/env bash
# The `prim "c"` mechanism's own checks (docs/ffi-decision.md), the ones the
# corpus cannot make:
#
#   kernels     runtime/tests/ffitest_kernels_test.c, the test kernels called
#               directly with canaries around every output: gcc and clang,
#               -O0 and -O2, natively (Linux); and the other architecture
#               under qemu-user when clang, an lld and qemu are all present
#               (skipped, and said so, otherwise)
#   symbols     every `prim "c"` declared in lib/ and runtime/tests has a
#               definition in EACH architecture's assembly, so a missing
#               kernel is not first found on the architecture nobody built
#   absent      a target without kernels (-DRT_HAVE_KERNELS=0) gets no call:
#               an `int` kernel answers -1
#   spawn       a spawned function reaching a `nonblocking` kernel is not warned
#               about; one reaching a kernel without the marker is
#   hook        the compiler's test module is not importable without
#               M31C_FFITEST
set -uo pipefail
cd "$(dirname "$0")/../.."
. ./config.sh
. ./runtime/arch.sh

LANGC=${LANGC:-./target/debug/$LANG_BIN}
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
fail=0
say() { printf '%-52s %s\n' "$1" "$2"; }
bad() { say "$1" FAILED; shift; printf '    %s\n' "$@"; fail=1; }

arch=$(uname -m)
case $arch in arm64) arch=aarch64 ;; amd64) arch=x86_64 ;; esac

# --- kernels ----------------------------------------------------------------
FREE=(-Wall -Wextra -ffreestanding -fno-stack-protector -fno-builtin -nostdlib -static)
if [ "$(uname -s)" = Linux ]; then
    for cc in gcc clang; do
        command -v "$cc" >/dev/null || continue
        for opt in -O0 -O2; do
            l="kernels $arch $cc $opt"
            if ! "$cc" "$opt" "${FREE[@]}" -o "$WORK/k" runtime/tests/ffitest_kernels_test.c \
                 "runtime/ffitest_$arch.S" "runtime/accel_$arch.S" "runtime/crypto_$arch.S" 2>"$WORK/k.err" || [ -s "$WORK/k.err" ]; then
                bad "$l" "did not build" "$(head -3 "$WORK/k.err")"
            elif [ "$("$WORK/k" 2>&1)" != ok ]; then
                bad "$l" "$("$WORK/k" 2>&1 | head -3)"
            else
                say "$l" ok
            fi
        done
    done
    # The other architecture, as sys_test.sh does it: clang cross-assembles,
    # the Rust toolchain's own lld links, qemu-user runs.
    lld=""
    if command -v ld.lld >/dev/null; then
        lld=$(command -v ld.lld)
    elif command -v rustc >/dev/null; then
        host=$(rustc -vV | sed -n 's/^host: //p')
        cand="$(rustc --print sysroot)/lib/rustlib/$host/bin/gcc-ld/ld.lld"
        [ -x "$cand" ] && lld=$cand
    fi
    for target in x86_64 aarch64; do
        [ "$target" = "$arch" ] && continue
        qemu=""
        for q in "qemu-$target" "qemu-$target-static"; do
            command -v "$q" >/dev/null && { qemu=$q; break; }
        done
        l="kernels $target clang under qemu"
        missing=""
        command -v clang >/dev/null || missing="clang"
        [ -n "$lld" ] || missing="$missing an lld"
        [ -n "$qemu" ] || missing="$missing qemu-$target"
        if [ -n "$missing" ]; then
            say "$l" "skipped (no$missing)"
            continue
        fi
        if ! clang --target="$target-linux-gnu" -O2 "${FREE[@]}" --ld-path="$lld" -o "$WORK/kx" \
             runtime/tests/ffitest_kernels_test.c "runtime/ffitest_$target.S" "runtime/accel_$target.S" \
             "runtime/crypto_$target.S" 2>"$WORK/kx.err" \
             || [ -s "$WORK/kx.err" ]; then
            bad "$l" "did not build" "$(head -3 "$WORK/kx.err")"
        elif [ "$("$qemu" "$WORK/kx" 2>&1)" != ok ]; then
            bad "$l" "$("$qemu" "$WORK/kx" 2>&1 | head -3)"
        else
            say "$l" ok
        fi
    done
else
    say "kernels (freestanding, Linux only)" "skipped ($(uname -s))"
fi

# --- symbols ----------------------------------------------------------------
declared=$(cat lib/*.m31 lib/*/*.m31 runtime/tests/ffitest.m31 | tr '\n' ' ' \
    | grep -o 'prim "c"[^(]*__k_[A-Za-z0-9_]*' | sed 's/.*__k_/rt_k_/' | sort -u)
if [ -z "$declared" ]; then
    bad "symbols" "no \`prim \"c\"\` declaration found: the scan is broken"
else
    for a in x86_64 aarch64; do
        # Every kernel file for this architecture is #included by its
        # ctx_switch file; find every file it names, one level down.
        files=$(sed -n 's/^#include "\(.*\)"/\1/p' "runtime/ctx_switch_$a.S")
        defined=$(for f in $files; do cat "runtime/$f"; done \
            | sed -n 's/^[A-Z_]*FUNC(\(rt_k_[A-Za-z0-9_]*\)).*/\1/p' | sort -u)
        missing=$(comm -23 <(echo "$declared") <(echo "$defined"))
        if [ -n "$missing" ]; then
            bad "symbols $a" "declared by a \`prim \"c\"\` and not defined:" $missing
        else
            say "symbols $a" "ok ($(echo "$declared" | wc -l | tr -d ' ') declared)"
        fi
    done
fi

# --- absent -----------------------------------------------------------------
cat >"$WORK/absent.$LANG_EXT" <<'EOP'
import ffitest;
print(ffitest.mix(1, 2, 3, 4, 5, 6));
EOP
if ! M31C_FFITEST=1 "$LANGC" -emit-c "$WORK/absent.$LANG_EXT" --out "$WORK/absent.c" 2>"$WORK/absent.err"; then
    bad "absent" "compile failed: $(head -1 "$WORK/absent.err")"
else
    for cc in gcc clang; do
        command -v "$cc" >/dev/null || continue
        for opt in -O0 -O2; do
            l="absent $cc $opt"
            if ! "$cc" "$opt" -Wall -Wextra -DRT_HAVE_KERNELS=0 -I runtime -pthread -o "$WORK/absent" \
                 "$WORK/absent.c" runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" 2>"$WORK/absent.cc" \
                 || [ -s "$WORK/absent.cc" ]; then
                bad "$l" "did not build" "$(head -3 "$WORK/absent.cc")"
            elif [ "$("$WORK/absent")" != "-1" ]; then
                bad "$l" "an int kernel on a target without kernels must answer -1, got: $("$WORK/absent")"
            else
                say "$l" ok
            fi
        done
    done
fi

# --- spawn ------------------------------------------------------------------
cat >"$WORK/sp_fast.$LANG_EXT" <<'EOP'
import ffitest;
void work() {
    print(ffitest.mix(1, 2, 3, 4, 5, 6));
}
spawn work();
EOP
cat >"$WORK/sp_slow.$LANG_EXT" <<'EOP'
import ffitest;
void work() {
    bytes b = [1, 2];
    print(ffitest.scale(1, b, 2));
}
spawn work();
EOP
M31C_FFITEST=1 "$LANGC" -emit-c "$WORK/sp_fast.$LANG_EXT" --out /dev/null 2>"$WORK/sp_fast.err"
M31C_FFITEST=1 "$LANGC" -emit-c "$WORK/sp_slow.$LANG_EXT" --out /dev/null 2>"$WORK/sp_slow.err"
if [ -s "$WORK/sp_fast.err" ]; then
    bad "spawn nonblocking" "a warning for a nonblocking kernel: $(head -1 "$WORK/sp_fast.err")"
elif ! grep -q 'may reach the foreign call' "$WORK/sp_slow.err"; then
    bad "spawn blocking" "no warning for a kernel without the marker"
else
    say "spawn" ok
fi

# --- hook -------------------------------------------------------------------
cat >"$WORK/hook.$LANG_EXT" <<'EOP'
import ffitest;
print(ffitest.mix(1, 2, 3, 4, 5, 6));
EOP
if env -u M31C_FFITEST "$LANGC" -emit-c "$WORK/hook.$LANG_EXT" --out /dev/null 2>"$WORK/hook.err"; then
    bad "hook" "ffitest imported without M31C_FFITEST"
elif ! grep -q 'cannot find module `ffitest`' "$WORK/hook.err"; then
    bad "hook" "wrong diagnostic: $(head -1 "$WORK/hook.err")"
else
    say "hook" ok
fi

# The test module is held to the library's own formatting, which the gate over
# lib/ does not see.
if ! M31C_FFITEST=1 "$LANGC" -fmt --check runtime/tests/ffitest.m31 >"$WORK/fmt.out" 2>&1; then
    bad "format" "runtime/tests/ffitest.m31 is not formatted: $(head -2 "$WORK/fmt.out")"
else
    say "format" ok
fi

exit $fail
