#!/usr/bin/env bash
# The markdown app's own tests. Builds, then runs every tests/*.md through the
# program and diffs against its tests/*.html.
#
#   bash apps/markdown/test.sh          every case
#   bash apps/markdown/test.sh links    only the cases whose name matches
#
# Where a .html came from: `reference.py` derives it from Python `commonmark`,
# the spec's reference implementation, except for a case with a `.hand` file
# beside it, which is written by hand and says so.
#
# The last check is speed, on a generated megabyte. It is a check that the
# program is not accidentally quadratic, not a benchmark.
set -uo pipefail
cd "$(dirname "$0")/../.."

# The "deep-quote" pathological case below (20,000 nested blockquotes) needs
# more than the runtime's 1 MiB default stack -- see runtime/greenthread.h's
# own comment on RT_STACK_SIZE for why the default stays 1 MiB everywhere
# else and this app asks for more itself instead. 8 MiB matches a typical OS
# thread's own default stack size, and is plenty of margin (empirically,
# deep-quote passes cleanly at 8 MiB where 1 MiB started failing around
# ~4,500 levels).
export LANG_STACK_SIZE=${LANG_STACK_SIZE:-8388608}

BIN=apps/markdown/markdown
T=apps/markdown/tests
filter=${1:-}

bash apps/markdown/build.sh >/dev/null || exit 1

pass=0
fail=0
for md in "$T"/*.md; do
    name=$(basename "$md" .md)
    [ -n "$filter" ] && [[ $name != *$filter* ]] && continue
    want="$T/$name.html"
    if [ ! -e "$want" ]; then
        printf '\033[31mFAIL\033[0m %-20s no expectation (run reference.py)\n' "$name"
        fail=$((fail + 1))
        continue
    fi
    # Byte for byte, through `cmp`: `$(...)` would strip trailing newlines and
    # the trailing newline is part of what cmark's layout promises.
    if "$BIN" "$md" | cmp -s - "$want"; then
        tag=""
        [ -e "$T/$name.hand" ] && tag=" (hand-written)"
        printf '\033[32mok\033[0m   %-20s%s\n' "$name" "$tag"
        pass=$((pass + 1))
    else
        printf '\033[31mFAIL\033[0m %-20s\n' "$name"
        diff "$want" <("$BIN" "$md") | head -20 | sed 's/^/       /'
        fail=$((fail + 1))
    fi
done

# --- stdin, --full, --out, a missing file: the command line, not the parser --
if [ -z "$filter" ]; then
    check() {
        local what=$1 want=$2 got=$3
        if [ "$want" = "$got" ]; then
            printf '\033[32mok\033[0m   %-20s\n' "$what"
            pass=$((pass + 1))
        else
            printf '\033[31mFAIL\033[0m %-20s want [%s] got [%s]\n' "$what" "$want" "$got"
            fail=$((fail + 1))
        fi
    }
    check "stdin" "<p>hi</p>" "$(printf '# x\n\nhi\n' | $BIN | tail -1)"
    check "dash is stdin" "<h1>x</h1>" "$(printf '# x\n' | $BIN - )"
    check "--full" "<!DOCTYPE html>" "$(printf 'x\n' | $BIN --full | head -1)"
    check "--title" "<title>T &amp; U</title>" \
          "$(printf 'x\n' | $BIN --full --title 'T & U' | sed -n 5p)"
    tmp=$(mktemp)
    $BIN --out "$tmp" "$T/headings.md" >/dev/null
    check "--out" "$(head -1 "$T/headings.html")" "$(head -1 "$tmp")"
    rm -f "$tmp"
    $BIN /nonexistent.md >/dev/null 2>&1
    check "missing file exits 1" "1" "$?"
    $BIN --nosuchoption </dev/null >/dev/null 2>&1
    check "bad option exits 2" "2" "$?"
fi

# --- differential fuzz, when the oracle is installed -------------------------
if [ -z "$filter" ] && python3 -c "import commonmark" 2>/dev/null; then
    for seed in 1 2 3 4; do
        if out=$(python3 apps/markdown/fuzz.py "$seed" 250) && [[ $out == *"0 mismatched, 0 crashed" ]]; then
            printf '\033[32mok\033[0m   %-20s %s\n' "fuzz seed $seed" "$out"
            pass=$((pass + 1))
        else
            printf '\033[31mFAIL\033[0m %-20s\n' "fuzz seed $seed"
            echo "$out" | head -20 | sed 's/^/       /'
            fail=$((fail + 1))
        fi
    done
else
    [ -z "$filter" ] && echo "skip fuzz: python3 -m pip install commonmark"
fi

# --- pathological inputs -----------------------------------------------------
#
# Not correctness: these are the shapes that turn a parser quadratic or
# overflow its stack. 50 000 unmatched `[` used to take 4.5 seconds, because
# every one of them searched forward for a `]` that was not there; the
# bracket map in `inlines.brackets` is why it is 7 ms now. The generous
# bounds are so this fails on an algorithm, not on a slow machine.
if [ -z "$filter" ]; then
    P=$(mktemp -d)
    python3 - "$P" <<'PY'
import sys
d = sys.argv[1]
open(d + "/deep-quote.md", "w").write("> " * 20000 + "hi\n")
open(d + "/deep-list.md", "w").write("".join("  " * i + "- item\n" for i in range(400)))
open(d + "/many-delims.md", "w").write("*a " * 20000 + "\n")
open(d + "/long-line.md", "w").write("x" * 2000000 + "\n")
open(d + "/backticks.md", "w").write("`" * 50000 + "\n")
open(d + "/brackets.md", "w").write("[" * 50000 + "\n")
open(d + "/images.md", "w").write("![" * 20000 + "\n")
PY
    for p in "$P"/*.md; do
        name=$(basename "$p" .md)
        start=$(date +%s%N)
        if ! "$BIN" "$p" >/dev/null 2>&1; then
            printf '\033[31mFAIL\033[0m %-20s exited non-zero\n' "pathological $name"
            fail=$((fail + 1))
            continue
        fi
        ms=$(( ($(date +%s%N) - start) / 1000000 ))
        if [ "$ms" -lt 2000 ]; then
            printf '\033[32mok\033[0m   %-20s %s ms\n' "pathological $name" "$ms"
            pass=$((pass + 1))
        else
            printf '\033[31mFAIL\033[0m %-20s %s ms -- something is quadratic\n' "pathological $name" "$ms"
            fail=$((fail + 1))
        fi
    done
    rm -rf "$P"
fi

# --- speed -------------------------------------------------------------------
if [ -z "$filter" ]; then
    big=$(mktemp /tmp/mdbig.XXXXXX.md)
    python3 - "$big" <<'PY'
import sys
one = """## Section

A paragraph with *emphasis*, **strong**, `code` and a [link](/a/b?c=1&d=2).
Another line of the same paragraph, with some non-ASCII: café 中文 👍.

- item one
- item two
  - nested item

> a quote

```c
int main(void) { return 0; }
```

"""
with open(sys.argv[1], "w") as f:
    while f.tell() < 1024 * 1024:
        f.write(one)
PY
    bytes=$(wc -c <"$big")
    start=$(date +%s%N)
    out=$($BIN "$big" | wc -c)
    ms=$(( ($(date +%s%N) - start) / 1000000 ))
    rm -f "$big"
    if [ "$ms" -lt 5000 ]; then
        printf '\033[32mok\033[0m   %-20s %s bytes in -> %s bytes out, %s ms\n' \
               "1 MB document" "$bytes" "$out" "$ms"
        pass=$((pass + 1))
    else
        printf '\033[31mFAIL\033[0m %-20s %s ms for %s bytes\n' "1 MB document" "$ms" "$bytes"
        fail=$((fail + 1))
    fi
fi

echo "── $pass passed, $fail failed"
[ $fail -eq 0 ]
