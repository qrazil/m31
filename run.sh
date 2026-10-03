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
. ./runtime/arch.sh

LANGC=${LANGC:-./target/debug/$LANG_BIN}
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
pass=0
fail=0
skipped=0

# -ffp-contract=off on every build: without it a C compiler may fuse
# `a * b + c` into one fused multiply-add, which rounds once instead of twice.
# That changes float results between targets that have FMA and targets that
# do not, and breaks the exact-arithmetic tricks lib/math.m31 relies on. x86-64
# without -march has no FMA, so the flag changes nothing here today; it is
# here so an ARM build cannot quietly disagree.
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

# Extra flags for compiling the runtime, and the way to pick its sys-layer
# backend: `RT_CFLAGS=-DRT_SYS_RAW bash run.sh` runs the whole corpus on raw
# Linux system calls instead of the C library (docs/sys-layer.md). Left
# unquoted where it is used, on purpose, so it can carry several flags.
RT_CFLAGS=${RT_CFLAGS:-}
[ -n "$RT_CFLAGS" ] && echo "runtime flags: $RT_CFLAGS"

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
  local base ref="" ref_tag="" entry cc opt bin got rc line
  base=$(basename "$src" ".$LANG_EXT")

  # A test's own `.skip-platform` file names the one `uname -s` value it can
  # run under at all (verbatim, e.g. "Linux") -- for a test whose whole point
  # is a platform-specific introspection technique with no portable
  # equivalent (grep its own header comment for which and why), not a stand-
  # in for actually porting the behavior. Skipped, not passed or failed: the
  # project's own stated rule ("a gate that can only report success is not a
  # gate") means this has to stay visible in the final tally, not vanish
  # into "passed" silently.
  local skip_file="${src%.$LANG_EXT}.skip-platform"
  if [ -e "$skip_file" ]; then
    local want_platform
    want_platform=$(cat "$skip_file")
    if [ "$(uname -s)" != "$want_platform" ]; then
      printf '\033[33mSKIP\033[0m %s (needs %s)\n' "$label" "$want_platform"
      skipped=$((skipped + 1))
      return
    fi
  fi

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
    # `spawn`/`Chan` always route through the Phase 1-3 green-thread runtime
    # now, so every build links the scheduler, the reactor, and the x86-64
    # context switch they share, alongside rt.c.
    if ! "$cc" "$opt" -ffp-contract=off -Wall -Wextra -DRC_DEBUG $RT_CFLAGS -I runtime \
         -pthread -o "$bin" "$WORK/$base.c" \
         runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" \
         2>"$WORK/$base.cc"; then
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

    # Standard input is the test's `.in` file if it has one, and empty
    # otherwise -- never the terminal, where a program reading a line would
    # hang the whole run.
    stdin="$PWD/${src%.$LANG_EXT}.in"
    [ -e "$stdin" ] || stdin=/dev/null
    # Command-line arguments are the test's `.args` file, one per line, and
    # none otherwise -- the same shape as `.in`, for `os.args()`. Expanded
    # below as "${argv[@]+"${argv[@]}"}" rather than the plain "${argv[@]}"
    # every other array in this file uses: bash's own nounset handling of
    # a genuinely EMPTY array (the common case here -- most tests have no
    # `.args` file) was only fixed in bash 4.4 (2016); macOS ships 3.2.57
    # (2007, pre-GPLv3) as /bin/bash to this day, where it is "unbound
    # variable" under `set -u` -- found for real on a macOS CI run, failing
    # every corpus test with no `.args` file at once. The other arrays in
    # this script (CCS, and gates.sh's/sanitize.sh's own non-argv arrays)
    # are always populated before use and never hit this.
    argv=()
    if [ -e "${src%.$LANG_EXT}.args" ]; then
        # Not `mapfile -t argv <file`: mapfile is a bash 4.0+ builtin, and
        # macOS's /bin/bash (3.2.57, 2007, pre-GPLv3) does not have it at
        # all -- found for real on a macOS CI run, "mapfile: command not
        # found". `|| [ -n "$line" ]` on the loop condition is what makes
        # this match mapfile's own behavior on a final line with no
        # trailing newline: `read` returns failure there but still
        # populates `$line`, so without this the loop would silently drop
        # that last line.
        #
        # `LC_ALL=C` scoped onto just this `read`, not the whole script, is
        # also load-bearing, not cosmetic: corpus/modules/stdlib-os-not-
        # utf8's own .args file has a line ending in a raw, non-UTF8 byte
        # (0xE9, deliberately -- that test is ABOUT args_bytes() handling
        # exactly that), and under a UTF-8 locale (en_US.UTF-8, what this
        # host and most CI runners default to) bash's own `read` applies
        # multibyte-aware line-ending detection that gets confused by that
        # invalid byte -- it silently fails to strip the trailing newline
        # for that one line, which a plain `diff` against that test's own
        # committed .out then reports as "prints something else", a 0a byte
        # that should not be there. Caught locally, by this exact test,
        # before this ever reached CI a second time; `mapfile` itself does
        # not have this failure mode, which is how the original version of
        # this fix missed it.
        while IFS= LC_ALL=C read -r line || [ -n "$line" ]; do
            argv+=("$line")
        done <"${src%.$LANG_EXT}.args"
    fi
    # A test's `.setup` script, if it has one, builds a fixture the program
    # cannot build for itself -- a file whose name is not valid UTF-8, which
    # no `str` path can spell -- in a fresh directory, and the program runs
    # there. Fresh for every build, so one run cannot see another's leftovers.
    rundir=$PWD
    if [ -e "${src%.$LANG_EXT}.setup" ]; then
      rundir="$WORK/$base.run"
      rm -rf "$rundir"
      mkdir -p "$rundir"
      if ! (cd "$rundir" && bash "$OLDPWD/${src%.$LANG_EXT}.setup") >"$WORK/$base.setup" 2>&1; then
        fail_test "$label [$cc $opt]" "setup failed: $(head -1 "$WORK/$base.setup")"
        return
      fi
    fi
    got=$(cd "$rundir" && "$bin" "${argv[@]+"${argv[@]}"}" 2>&1 <"$stdin")
    rc=$?

    # Layer 4: refcount invariant. The runtime prints this under -DRC_DEBUG.
    if ! grep -q '^__rc_live=0$' <<<"$got"; then
      fail_test "$label [$cc $opt]" "leak: $(grep '^__rc_live=' <<<"$got" || echo 'no marker emitted')"
      return
    fi
    got=$(grep -v '^__rc_live=' <<<"$got")

    # The exit status is 0 unless the test's `.status` file says otherwise,
    # for `os.exit(code)`.
    want_rc=0
    [ -e "${src%.$LANG_EXT}.status" ] && want_rc=$(cat "${src%.$LANG_EXT}.status")
    if [ $rc -ne "$want_rc" ]; then
      fail_test "$label [$cc $opt]" "exited $rc, expected $want_rc"
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
    if ! "$cc" "$opt" -ffp-contract=off -Wall -Wextra -DRC_DEBUG $RT_CFLAGS -I runtime \
         -pthread -o "$bin" "$WORK/$base.c" \
         runtime/rt.c runtime/scheduler.c "$RT_REACTOR_C" "$RT_CTX_ASM" \
         2>"$WORK/$base.tcc"; then
      fail_test "$label [$cc $opt]" "C compiler rejected emitted code"
      trap_ok=0; break
    fi
    if [ -s "$WORK/$base.tcc" ]; then
      fail_test "$label [$cc $opt]" "emitted C produced warnings"
      head -4 "$WORK/$base.tcc" | sed 's/^/      /'
      trap_ok=0; break
    fi
    # A trap test may have a `.args` file, the same shape as a program
    # test's: the one way to reach a trap that only a command line can cause.
    targv=()
    if [ -e "${src%.$LANG_EXT}.args" ]; then
        # See run_one's own matching comment for why not `mapfile`.
        while IFS= LC_ALL=C read -r line || [ -n "$line" ]; do
            targv+=("$line")
        done <"${src%.$LANG_EXT}.args"
    fi
    got=$("$bin" "${targv[@]+"${targv[@]}"}" 2>&1 >/dev/null </dev/null); rc=$?
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

if [ "$skipped" -gt 0 ]; then
  echo "── $pass passed, $fail failed, $skipped skipped (platform-specific)"
else
  echo "── $pass passed, $fail failed"
fi
[ $fail -eq 0 ]
