#!/usr/bin/env bash
# Differential corpus runner.
#
# Four oracle layers, all of which must agree:
#
#   1. gcc vs clang        — disagreement means the emitted C relies on UB
#   2. -O0 vs -O2          — optimisation must not change observable behaviour
#   3. Go twin programs    — genuine external truth, written by other people
#   4. refcount invariant  — zero live objects at exit, no decrement below zero
#
# Exits non-zero on any failure. Until the compiler exists, everything fails;
# that is correct. The first "1 passed" is the walking-skeleton milestone.
set -uo pipefail
cd "$(dirname "$0")"
. ./config.sh

LANGC=${LANGC:-./target/debug/$LANG_BIN}
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
pass=0
fail=0

# --- compiler matrix: whatever is installed ---------------------------------
CCS=()
command -v gcc   >/dev/null && CCS+=("gcc:-O0" "gcc:-O2")
command -v clang >/dev/null && CCS+=("clang:-O0" "clang:-O2")
if [ ${#CCS[@]} -eq 0 ]; then
  echo "no C compiler found (need gcc and/or clang)" >&2
  exit 1
fi
if [ ${#CCS[@]} -lt 4 ]; then
  echo "note: only one C compiler present — the gcc/clang UB differential is off"
fi
echo "matrix: ${CCS[*]}"

if [ ! -x "$LANGC" ]; then
  echo "compiler not built: $LANGC" >&2
  echo "(expected until the walking skeleton lands)" >&2
  exit 1
fi

fail_test() {
  printf '\033[31mFAIL\033[0m %s\n      %s\n' "$1" "$2"
  fail=$((fail + 1))
}

# run_one <source> <expected-stdout> <label>
run_one() {
  local src=$1 expected=$2 label=$3
  local base ref="" ref_tag="" entry cc opt bin got rc
  base=$(basename "$src" ".$LANG_EXT")

  if ! "$LANGC" --emit-c "$src" -o "$WORK/$base.c" 2>"$WORK/$base.diag"; then
    fail_test "$label" "compile failed: $(head -1 "$WORK/$base.diag")"
    return
  fi

  for entry in "${CCS[@]}"; do
    cc=${entry%%:*}
    opt=${entry##*:}
    bin="$WORK/$base.$cc$opt"

    # The runtime is a separate translation unit on purpose, and -flto is
    # deliberately absent — see docs/ir-v0.md §7.1 and runtime/rt.c.
    if ! "$cc" "$opt" -Wall -Wextra -DRC_DEBUG -I runtime \
         -pthread -o "$bin" "$WORK/$base.c" runtime/rt.c -lm 2>"$WORK/$base.cc"; then
      fail_test "$label [$cc $opt]" "C compiler rejected emitted code: $(head -1 "$WORK/$base.cc")"
      return
    fi

    # Emitted C must be warning-free, same instinct as Oro's
    # `clippy -D warnings` gate. A warning in generated code is a defect in
    # the emitter, and it is usually the early form of a UB bug.
    if [ -s "$WORK/$base.cc" ]; then
      fail_test "$label [$cc $opt]" "emitted C produced warnings"
      head -4 "$WORK/$base.cc" | sed 's/^/      /'
      return
    fi

    got=$("$bin" 2>&1)
    rc=$?

    # Layer 4: refcount invariant. The runtime prints this under -DRC_DEBUG.
    if ! grep -q '^__rc_live=0$' <<<"$got"; then
      fail_test "$label [$cc $opt]" "leak: $(grep '^__rc_live=' <<<"$got" || echo 'no marker emitted')"
      return
    fi
    got=$(grep -v '^__rc_live=' <<<"$got")

    if [ $rc -ne 0 ]; then
      fail_test "$label [$cc $opt]" "exited $rc"
      return
    fi

    # Layers 1 and 2: every build must agree with every other build.
    if [ -z "$ref_tag" ]; then
      ref=$got
      ref_tag="$cc $opt"
    elif [ "$got" != "$ref" ]; then
      fail_test "$label" "$cc $opt disagrees with $ref_tag"
      diff <(echo "$ref") <(echo "$got") | head -5 | sed 's/^/      /'
      return
    fi
  done

  # Layer 3 (twin) or the hand-written expectation (core).
  if [ "$ref" != "$expected" ]; then
    fail_test "$label" "output differs from oracle"
    diff <(echo "$expected") <(echo "$ref") | head -5 | sed 's/^/      /'
    return
  fi

  pass=$((pass + 1))
}

# --- core: expected output is hand-authored ---------------------------------
for src in corpus/core/*."$LANG_EXT"; do
  [ -e "$src" ] || continue
  run_one "$src" "$(cat "${src%.$LANG_EXT}.out")" "core/$(basename "$src")"
done

# --- twin: expected output comes from Go ------------------------------------
for src in corpus/twin/*."$LANG_EXT"; do
  [ -e "$src" ] || continue
  twin="${src%.$LANG_EXT}.twin.go"
  if [ ! -e "$twin" ]; then
    fail_test "twin/$(basename "$src")" "missing $twin"
    continue
  fi
  run_one "$src" "$(go run "$twin")" "twin/$(basename "$src")"
done

# --- traps: must abort at runtime, with the expected trap message -----------
# abort() skips atexit, so a trapping program never prints __rc_live. The
# refcount invariant is therefore not checked here, which is why these are a
# separate category rather than a flavour of core/.
for src in corpus/traps/*."$LANG_EXT"; do
  [ -e "$src" ] || continue
  label="traps/$(basename "$src")"
  base=$(basename "$src" ".$LANG_EXT")
  want=$(cat "${src%.$LANG_EXT}.trap")

  if ! "$LANGC" --emit-c "$src" -o "$WORK/$base.c" 2>"$WORK/$base.diag"; then
    fail_test "$label" "compile failed: $(head -1 "$WORK/$base.diag")"
    continue
  fi

  trap_ok=1
  for entry in "${CCS[@]}"; do
    cc=${entry%%:*}
    opt=${entry##*:}
    bin="$WORK/$base.t.$cc$opt"
    if ! "$cc" "$opt" -Wall -Wextra -DRC_DEBUG -I runtime \
         -pthread -o "$bin" "$WORK/$base.c" runtime/rt.c -lm 2>"$WORK/$base.tcc"; then
      fail_test "$label [$cc $opt]" "C compiler rejected emitted code"
      trap_ok=0; break
    fi
    if [ -s "$WORK/$base.tcc" ]; then
      fail_test "$label [$cc $opt]" "emitted C produced warnings"
      head -4 "$WORK/$base.tcc" | sed 's/^/      /'
      trap_ok=0; break
    fi
    got=$("$bin" 2>&1 >/dev/null); rc=$?
    if [ $rc -ne 134 ]; then
      fail_test "$label [$cc $opt]" "expected abort (134), got exit $rc"
      trap_ok=0; break
    fi
    if [ "$got" != "$want" ]; then
      fail_test "$label [$cc $opt]" "wrong trap message"
      diff <(echo "$want") <(echo "$got") | head -4 | sed 's/^/      /'
      trap_ok=0; break
    fi
  done
  [ $trap_ok -eq 1 ] && pass=$((pass + 1))
done

# --- errors: must fail to compile, with the expected diagnostic -------------
for src in corpus/errors/*."$LANG_EXT"; do
  [ -e "$src" ] || continue
  label="errors/$(basename "$src")"
  if "$LANGC" --emit-c "$src" -o /dev/null 2>"$WORK/e.diag"; then
    fail_test "$label" "compiled, but must be rejected"
  elif ! diff -q "${src%.$LANG_EXT}.err" "$WORK/e.diag" >/dev/null 2>&1; then
    fail_test "$label" "wrong diagnostic"
    diff "${src%.$LANG_EXT}.err" "$WORK/e.diag" 2>/dev/null | head -5 | sed 's/^/      /'
  else
    pass=$((pass + 1))
  fi
done

# --- modules: a program spread over several files ---------------------------
#
# One directory per case, entry point `main.$LANG_EXT`, and beside it either
# `main.out` for a program that runs or `main.err` for one that must be
# refused. Nested a level deeper than the flat categories so that the other
# globs do not pick a library file up and run it on its own.
for dir in corpus/modules/*/; do
  [ -d "$dir" ] || continue
  case_name=$(basename "$dir")
  dir=${dir%/}              # the glob leaves a trailing slash
  src="$dir/main.$LANG_EXT"
  if [ ! -e "$src" ]; then
    fail_test "modules/$case_name" "no main.$LANG_EXT"
    continue
  fi
  if [ -e "$dir/main.err" ]; then
    label="modules/$case_name"
    if "$LANGC" --emit-c "$src" -o /dev/null 2>"$WORK/m.diag"; then
      fail_test "$label" "compiled, but must be rejected"
    else
      # Diagnostics name the file they came from, and that path depends on
      # where the corpus sits. Compare from the corpus root down.
      sed -E "s#^.*/(corpus/)#\\1#" "$WORK/m.diag" > "$WORK/m.norm"
      if ! diff -q "$dir/main.err" "$WORK/m.norm" >/dev/null 2>&1; then
        fail_test "$label" "wrong diagnostic"
        diff "$dir/main.err" "$WORK/m.norm" 2>/dev/null | head -5 | sed 's/^/      /'
      else
        pass=$((pass + 1))
      fi
    fi
  else
    run_one "$src" "$(cat "$dir/main.out")" "modules/$case_name"
  fi
done

echo "── $pass passed, $fail failed"
[ $fail -eq 0 ]
