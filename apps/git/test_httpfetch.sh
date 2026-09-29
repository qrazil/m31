# `httpfetch.src`: git's smart-HTTP protocol, against a real `git
# http-backend` -- the task this file exists for names it plainly: "stand up
# your own real smart-HTTP git server as the test oracle", never a hand-rolled
# stand-in, so this is `git http-backend` run as a genuine CGI script behind
# Python's `http.server.CGIHTTPRequestHandler`, serving a disposable bare
# repository this script builds itself. Nothing here ever touches a real
# remote or anyone else's server.
#
# Sourced from `test.sh`, sharing its shell, `$WORK`, `build`, `note`/`bad`
# and the `pass`/`fail` counters, exactly as `test_write.sh`'s own header
# describes. **This is the one file in the suite that sets a `trap`**: every
# other sourced script reads fixtures it built itself, but this one also
# starts a real background server process, and that process must be killed
# by its own exact PID before the script exits -- never `pkill`, never by
# name -- whether every check below passes or one of them fails partway.
# The trap composes with `test.sh`'s own (`rm -rf "$WORK"`) rather than
# replacing its effect: it is rewritten, once, to do both.
#
# What each check proves, in the order `apps/git/design.md`'s task asks for:
#
#   1. the ref advertisement matches `git ls-remote` byte for byte;
#   2. a full clone (no `have`s) produces a verified pack that is byte for
#      byte the server's own delta-compressed pack, and real
#      `git index-pack --stdin` accepts it;
#   3. a real `git clone` seeds a local repository, the fixture gains a new
#      commit, and a `fetch` computing `have`s from that local repository
#      gets a smaller pack containing only the new objects -- negotiation
#      doing something, not just refetching everything -- again accepted by
#      real `index-pack`;
#   4. truncated, checksum-flipped, too-short and non-pack bytes are all
#      refused with nothing written to disk, checked directly against
#      `verify_pack` with no server involved (the task's own "simulate this
#      at the test level"); and, for the strongest version of the same
#      claim, a genuinely truncated response over the wire -- a small
#      corrupting proxy in front of the real server, adjusting
#      `Content-Length` to match so it is this module's own checksum check
#      catching it and not `lib/http.src`'s transport-level framing.

hf_root="$WORK/httpfetch"
mkdir -p "$hf_root"

# --- 1. the source working tree: a real history worth repacking -------------
#
# Eight commits sharing enough content to delta well, a second branch two
# commits back, and an annotated tag -- so `git repack -ad` downstream
# produces genuine `OBJ_*_DELTA` objects, not just whole blobs.

hf_src="$hf_root/src"
mkdir -p "$hf_src"
(
    set -e
    cd "$hf_src"
    git init -q -b main .
    git config user.email httpfetch@example.com
    git config user.name "HTTP Fetch Tester"
    i=1
    while [ "$i" -le 8 ]; do
        printf 'line %d\nshared boilerplate content that repeats across commits\nmore filler text making deltas worthwhile\n' "$i" >>bigfile.txt
        echo "content $i" >"file$i.txt"
        git add -A
        GIT_AUTHOR_DATE="170000000$i +0000" GIT_COMMITTER_DATE="170000000$i +0000" \
            git commit -q -m "commit $i"
        i=$((i + 1))
    done
    git branch feature main~2
    git checkout -q feature
    echo "on the feature branch" >feature.txt
    git add -A
    GIT_AUTHOR_DATE="1700000099 +0000" GIT_COMMITTER_DATE="1700000099 +0000" \
        git commit -q -m "feature commit"
    git checkout -q main
    git tag -a v1.0 -m "release 1.0" main~3
) >"$WORK/httpfetch-src.log" 2>&1 && note "httpfetch: source fixture built (8 commits, a branch, an annotated tag)" ||
    bad "httpfetch source fixture" "$(tail -8 "$WORK/httpfetch-src.log")"

# --- 2. the "server": a bare clone, repacked so the pack it sends has deltas

hf_srv="$hf_root/srv/repo.git"
if git clone -q --bare "$hf_src" "$hf_srv" >"$WORK/httpfetch-bare.log" 2>&1 &&
    git -C "$hf_srv" repack -ad -q >>"$WORK/httpfetch-bare.log" 2>&1 &&
    git -C "$hf_srv" gc -q --prune=now >>"$WORK/httpfetch-bare.log" 2>&1; then
    hf_srv_pack=$(ls "$hf_srv"/objects/pack/*.pack 2>/dev/null | head -1)
    hf_chains=$(git -C "$hf_srv" verify-pack -v "${hf_srv_pack%.pack}.idx" 2>/dev/null | grep -c "^chain length")
    note "httpfetch: server repo repacked ($hf_chains delta chain length(s) present)"
else
    bad "httpfetch server fixture" "$(tail -8 "$WORK/httpfetch-bare.log")"
fi

# --- 3. the real oracle: `git http-backend` as a genuine CGI script ---------

hf_git_exec=$(git --exec-path)
hf_backend="$hf_git_exec/git-http-backend"
hf_web="$hf_root/www"
mkdir -p "$hf_web/cgi-bin"

if command -v python3 >/dev/null 2>&1 && [ -x "$hf_backend" ]; then
    cat >"$hf_web/cgi-bin/git-http-backend" <<WRAP
#!/bin/sh
export GIT_PROJECT_ROOT="$hf_root/srv"
export GIT_HTTP_EXPORT_ALL=1
exec "$hf_backend"
WRAP
    chmod +x "$hf_web/cgi-bin/git-http-backend"

    cat >"$hf_root/serve.py" <<'PY'
import http.server
import os
import sys

port = int(sys.argv[1])
os.chdir(sys.argv[2])


class Handler(http.server.CGIHTTPRequestHandler):
    cgi_directories = ["/cgi-bin"]

    def log_message(self, fmt, *args):
        pass


http.server.HTTPServer(("127.0.0.1", port), Handler).serve_forever()
PY

    hf_port=$((20000 + (RANDOM % 20000)))
    hf_url="http://127.0.0.1:$hf_port/cgi-bin/git-http-backend/repo.git"

    python3 "$hf_root/serve.py" "$hf_port" "$hf_web" >"$WORK/httpfetch-server.log" 2>&1 &
    hf_server_pid=$!
    # Set now, empty, so the trap below never trips `set -u` on it before the
    # corrupting proxy (further down) gives it a real value.
    hf_proxy_pid=""
    # See the module header: this replaces test.sh's own EXIT trap with one
    # that also kills this exact PID (and, once it exists, the corrupting
    # proxy's), so `test.sh`'s `rm -rf "$WORK"` still happens either way.
    trap 'kill "$hf_server_pid" ${hf_proxy_pid:+"$hf_proxy_pid"} >/dev/null 2>&1; wait "$hf_server_pid" ${hf_proxy_pid:+"$hf_proxy_pid"} 2>/dev/null; rm -rf "$WORK"' EXIT

    hf_up=0
    hf_tries=0
    while [ "$hf_tries" -lt 50 ]; do
        if (exec 3<>"/dev/tcp/127.0.0.1/$hf_port") 2>/dev/null; then
            exec 3>&-
            hf_up=1
            break
        fi
        sleep 0.1
        hf_tries=$((hf_tries + 1))
    done

    if [ "$hf_up" = 1 ] && build t_httpfetch; then
        # --- the ref advertisement, byte for byte against 'git ls-remote' --

        "$WORK/t_httpfetch" ls-remote "$hf_url" >"$WORK/httpfetch-lsremote.got" 2>"$WORK/httpfetch-lsremote.err"
        git ls-remote "$hf_url" >"$WORK/httpfetch-lsremote.want" 2>"$WORK/httpfetch-lsremote.want.err"
        if cmp -s "$WORK/httpfetch-lsremote.got" "$WORK/httpfetch-lsremote.want"; then
            note "httpfetch: ref advertisement matches 'git ls-remote' byte for byte"
        else
            bad "httpfetch ls-remote" \
                "$(diff "$WORK/httpfetch-lsremote.got" "$WORK/httpfetch-lsremote.want")" \
                "$(cat "$WORK/httpfetch-lsremote.err")"
        fi

        # --- a full clone: no haves, verified, byte for byte the server's --
        # --- own pack, and accepted by real 'git index-pack --stdin' ------

        hf_clone_pack="$hf_root/clone.pack"
        if "$WORK/t_httpfetch" clone "$hf_url" "$hf_clone_pack" \
            >"$WORK/httpfetch-clone.out" 2>"$WORK/httpfetch-clone.err"; then
            if cmp -s "$hf_clone_pack" "$hf_srv_pack"; then
                note "httpfetch clone: pack is byte-for-byte identical to the server's own repacked pack"
            else
                bad "httpfetch clone bytes" "the fetched pack and the server's own pack differ"
            fi
            hf_idx1="$hf_root/idxcheck-clone"
            git init -q --bare "$hf_idx1"
            if git -C "$hf_idx1" index-pack --stdin <"$hf_clone_pack" \
                >"$WORK/httpfetch-idx1.out" 2>"$WORK/httpfetch-idx1.err"; then
                hf_srv_objs=$(git -C "$hf_srv" count-objects -v | grep "^in-pack" | awk '{print $2}')
                note "httpfetch clone: real 'git index-pack --stdin' accepts it ($hf_srv_objs objects, matching the source repo's own pack)"
            else
                bad "httpfetch clone: index-pack" "$(cat "$WORK/httpfetch-idx1.err")"
            fi
        else
            bad "httpfetch clone" "$(cat "$WORK/httpfetch-clone.out")" "$(cat "$WORK/httpfetch-clone.err")"
        fi

        # --- fetch with haves: advance the fixture, then confirm the pack --
        # --- shrinks to just the new objects -------------------------------

        hf_localclone="$hf_root/localclone"
        git clone -q "$hf_url" "$hf_localclone" >"$WORK/httpfetch-localclone.log" 2>&1
        (
            set -e
            cd "$hf_src"
            echo "advanced after the first clone" >advance.txt
            git add -A
            GIT_AUTHOR_DATE="1700000200 +0000" GIT_COMMITTER_DATE="1700000200 +0000" \
                git commit -q -m "commit 9 -- after the first clone"
            git push -q "$hf_srv" main
        ) >"$WORK/httpfetch-advance.log" 2>&1 || bad "httpfetch: advance the fixture" "$(tail -8 "$WORK/httpfetch-advance.log")"

        hf_fetch_pack="$hf_root/fetch.pack"
        if "$WORK/t_httpfetch" fetch "$hf_url" "$hf_localclone/.git" "$hf_fetch_pack" \
            >"$WORK/httpfetch-fetch.out" 2>"$WORK/httpfetch-fetch.err"; then
            hf_clone_size=$(wc -c <"$hf_clone_pack")
            hf_fetch_size=$(wc -c <"$hf_fetch_pack")
            hf_idx2="$hf_root/idxcheck-fetch"
            git init -q --bare "$hf_idx2"
            if git -C "$hf_idx2" index-pack --stdin <"$hf_fetch_pack" \
                >"$WORK/httpfetch-idx2.out" 2>"$WORK/httpfetch-idx2.err" &&
                [ "$hf_fetch_size" -lt "$hf_clone_size" ]; then
                note "httpfetch fetch (with haves): pack shrank $hf_clone_size -> $hf_fetch_size bytes and real index-pack accepts it -- negotiation is pruning, not resending everything"
            else
                bad "httpfetch fetch with haves" "clone=$hf_clone_size fetch=$hf_fetch_size" "$(cat "$WORK/httpfetch-idx2.err")"
            fi
        else
            bad "httpfetch fetch with haves" "$(cat "$WORK/httpfetch-fetch.out")" "$(cat "$WORK/httpfetch-fetch.err")"
        fi

        # --- corruption, at the test level: hand verify_pack bytes that ---
        # --- never went near a socket --------------------------------------

        python3 - "$hf_clone_pack" "$hf_root" <<'PY'
import sys

data = open(sys.argv[1], "rb").read()
root = sys.argv[2]
open(root + "/truncated.pack", "wb").write(data[:-10])
flipped = bytearray(data)
flipped[-1] ^= 0xFF
open(root + "/flipped.pack", "wb").write(bytes(flipped))
open(root + "/tiny.pack", "wb").write(data[:20])
open(root + "/notpack.pack", "wb").write(b"NOTAPACKFILEATALL!!!!!!" + b"\x00" * 20)
PY
        hf_corrupt_ok=1
        for hf_bad in truncated flipped tiny notpack; do
            if "$WORK/t_httpfetch" verify "$hf_root/$hf_bad.pack" "$hf_root/out-$hf_bad.pack" \
                >/dev/null 2>"$WORK/httpfetch-corrupt-$hf_bad.err"; then
                hf_corrupt_ok=0
            fi
            if [ -e "$hf_root/out-$hf_bad.pack" ]; then
                hf_corrupt_ok=0
            fi
        done
        if ! "$WORK/t_httpfetch" verify "$hf_clone_pack" "$hf_root/out-good.pack" \
            >/dev/null 2>"$WORK/httpfetch-corrupt-good.err" ||
            ! cmp -s "$hf_clone_pack" "$hf_root/out-good.pack"; then
            hf_corrupt_ok=0
        fi
        if [ "$hf_corrupt_ok" = 1 ]; then
            note "httpfetch: truncated/checksum-flipped/too-short/non-pack bytes are all refused with nothing written; a genuine pack still verifies and writes byte for byte"
        else
            bad "httpfetch corruption handling" "at least one corrupt case was accepted, or the good pack was not written correctly"
        fi

        # --- the same claim, over the wire: a corrupting proxy in front ---
        # --- of the real server, truncating only the upload-pack response -

        cat >"$hf_root/corrupt_proxy.py" <<PROXY
import http.client
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

UPSTREAM_PORT = $hf_port


class Proxy(BaseHTTPRequestHandler):
    def _relay(self, method):
        length = int(self.headers.get("Content-Length", 0) or 0)
        body = self.rfile.read(length) if length else b""
        conn = http.client.HTTPConnection("127.0.0.1", UPSTREAM_PORT)
        headers = {k: v for k, v in self.headers.items() if k.lower() not in ("host", "content-length")}
        conn.request(method, self.path, body=body if body else None, headers={**headers, "Content-Length": str(len(body))} if body else headers)
        resp = conn.getresponse()
        data = resp.read()
        if method == "POST" and self.path.endswith("/git-upload-pack"):
            data = data[:-30]
        self.send_response(resp.status)
        for k, v in resp.getheaders():
            if k.lower() in ("transfer-encoding", "content-length"):
                continue
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        self._relay("GET")

    def do_POST(self):
        self._relay("POST")

    def log_message(self, fmt, *args):
        pass


HTTPServer(("127.0.0.1", $((hf_port + 1))), Proxy).serve_forever()
PROXY
        python3 "$hf_root/corrupt_proxy.py" >"$WORK/httpfetch-proxy.log" 2>&1 &
        hf_proxy_pid=$!
        hf_proxy_up=0
        hf_tries=0
        while [ "$hf_tries" -lt 50 ]; do
            if (exec 4<>"/dev/tcp/127.0.0.1/$((hf_port + 1))") 2>/dev/null; then
                exec 4>&-
                hf_proxy_up=1
                break
            fi
            sleep 0.1
            hf_tries=$((hf_tries + 1))
        done
        if [ "$hf_proxy_up" = 1 ]; then
            hf_wire_url="http://127.0.0.1:$((hf_port + 1))/cgi-bin/git-http-backend/repo.git"
            hf_wire_out="$hf_root/should-not-exist.pack"
            "$WORK/t_httpfetch" clone "$hf_wire_url" "$hf_wire_out" \
                >"$WORK/httpfetch-wiretrunc.out" 2>"$WORK/httpfetch-wiretrunc.err"
            hf_wire_status=$?
            if [ "$hf_wire_status" -ne 0 ] && [ ! -e "$hf_wire_out" ] &&
                grep -q "cut off in the middle" "$WORK/httpfetch-wiretrunc.err"; then
                note "httpfetch: a genuinely truncated wire response is refused end to end, nothing written"
            else
                bad "httpfetch wire truncation" "$(cat "$WORK/httpfetch-wiretrunc.out")" "$(cat "$WORK/httpfetch-wiretrunc.err")"
            fi
        else
            bad "httpfetch wire truncation" "the corrupting proxy did not come up"
        fi
        kill "$hf_proxy_pid" >/dev/null 2>&1
        wait "$hf_proxy_pid" 2>/dev/null
    else
        bad "httpfetch" "the test server did not come up, or t_httpfetch did not build"
    fi
else
    bad "httpfetch" "skipped: python3 or a real 'git http-backend' is not available in this environment"
fi
