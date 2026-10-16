# CLI inventory of every m31 app (phase 1)

Audited 2026-10-15 from `origin/master` of each repo (m31 `0f4c4da`, m31git/gitui
`febc819`, m3dit `4d41a00`, term-markdown `e384cc7`, tui-markdown `2529f22`,
httpserver `8fe4116`, syntax `e49e01c`, tui `57a63af`, benchmark `32c77d6`), read in
the `feat/cli-*` worktrees that were cut from them. Branches that are still in
flight (m3dit `feat/themes`, `feat/smooth`; m31git `perf/1git`, `chore/m31git-rename`,
`feat/side-by-side`; tui-markdown `feat/md-editor`; m31 `feat/ssh-complete`) were
NOT audited except where a flag is known to be coming (noted in 6.3).

## 0. What "oro format" means: the spec, and how it was settled

"Oro format" is **Oro's `args` grammar**: `~/homelab/claude_bridge/workspace/oro/docs/reference.md`
lines 2180-2443 ("`args` -- written in Oro"), pinned case by case by
`corpus/divergence/110_args.oro`, chosen by the owner on 2026-09-17, and ported to m31 as
`lib/args.m31`:

* one dash is a **command** (`-serve`, short `-s`), two dashes an **option** (`--out PATH`,
  `--out=PATH`, short `--o`), anything else **data**;
* several commands per line run in order, the whole line validated first; two levels deep;
* `--help` is the only help; `-h`, `-help`, `help`, `--h`, a lone `--` are refused with a
  message naming what to write; shorts do not cluster; abbreviations are suggested, never
  accepted; no `--no-x`; no hidden options; no required options (positionals instead);
  list options are greedy; `-5`, `-1.5` and `-` are data.

The first pass of this audit misread the brief ("`-n` for proper options and `--number` for
fullnames") as the POSIX convention and built `lib/cli.m31` on it. The owner corrected it
(2026-10-16): the standard is Oro's, for **every** application, including the ones that
mimicked OpenSSH or git; a drop-in clone gives nobody a reason to use ours. That pass was
discarded (`lib/cli.m31` is deleted). The deviation lists in sections 2-4 below were
written against the POSIX draft and are kept as the factual record of what each program
accepted; **section 5 is the Oro count** and section 7 says what each branch did.

Rule codes used in sections 2-4 (the POSIX draft): R1 short form, R2 long form,
R3 value spellings, R4 `--`, R5 clustering, R6 `-h`, R7 `-V`, R8 same name for the same
concept, R9 exit codes (0 / 1 / 2), R10 usage errors on stderr with the usage line,
R11 closest-name suggestion, R12 repeats and conflicts, R13 positionals documented.
Under Oro R4-R7 are the opposite (refused or absent on purpose), R3 and R5 do not apply.

## 1. Executables and subcommands (the complete list)

| repo | executable | what | parser today |
|---|---|---|---|
| m31 | `m31c` | compiler driver (Rust) | hand-rolled `match` loops in `src/main.rs`, `src/tools/lint.rs`, `src/tools/init/mod.rs` |
| m31 | `m31ssh` | ssh client | hand-rolled (`apps/m31ssh/core/SSHC_cli.m31`) |
| m31 | `m31scp` | scp over SFTP | hand-rolled (`apps/m31scp/core/SCP_main.m31`) |
| m31 | `m31sftp` | sftp client | hand-rolled (`apps/m31sftp/core/SFTP_main.m31`) |
| m31 | `m31ssh-keygen` | ssh-keygen subset | hand-rolled (`apps/m31ssh-keygen/core/KEYGEN_main.m31`) |
| m31 | `install.sh` | installer (curl-pipe) | `$1 = --uninstall` and env vars |
| m31 | `build.sh`, `apps/build.sh`, `gates.sh`, `scripts/*.sh` | developer scripts, not shipped | positional + `-o` / `--quick` |
| m31git | `1git` (`git.m31`) | git drop-in: git grammar (exempt) + old `-word` plumbing + `--git-dir` | `lib/args` for plumbing; hand-rolled prefix loop + one `GIT_git*.m31` parser per git command |
| m31git | `mgit` (`gitui.m31`) | git TUI | `argv[1]` only |
| m31git | `install.sh`, `scripts/build.sh`, `scripts/compare*.sh` | installer, build, dev | env vars, `-o` |
| m3dit | `m3` (`m3.m31`) | modal editor | hand-rolled loop in `m3.m31` |
| m3dit | `install.sh`, `scripts/build.sh`, `scripts/gen_keymap.sh`, `scripts/oracle.sh` | | env, `-o` |
| httpserver | `httpserver` | hello / static file server | `lib/args` |
| term-markdown | `markdown` | markdown to HTML | `lib/args` + a pre-pass for a hidden `--watch-limit` |
| tui-markdown | `tui-markdown` | markdown viewer | `lib/args` (`MDV_cli.m31`) |
| tui | none shipped; `examples/{demo,browse,widgets,bench}.m31` | library; example programs take no arguments | none |
| syntax | none shipped; dev tools `tests/oracle/dump.m31`, `tests/tools/show.m31` | library | positional `argv[1..]`, unchecked |
| benchmark | `run.sh` | runs workloads across languages | `$1 = -n` only |

Subcommands:

* `m31c`: modes `--emit-c`, `--emit-ir` (mode flags, not subcommands), subcommands `fmt`, `lint`, `init`.
* `1git`: git commands (`status log show diff add rm mv reset commit switch checkout branch tag restore clone init remote ls-remote pull push fetch stash cherry-pick revert merge rebase clean worktree`), old plumbing commands `-cat-file -ls-tree -log -rev-parse -refs`.
* everything else: no subcommands (`sftp` has an interactive command language, see 2.5).

## 2. Per-app detail

Types: `int`, `str`, `path`, `enum`, `list`, `bool`. "rep" = repeatable.

### 2.1 m31c (Rust, `src/main.rs`, `src/tools/lint.rs`, `src/tools/init/mod.rs`)

Usage today (all three spellings differ):

    m31c --emit-c|--emit-ir|fmt <source> [-o <output>]
    m31c fmt [--check] [-r|--recursive] <dir|file>
    m31c lint [--allow NAME[,NAME..]] [--strict] [--summary] <file|dir>...
    m31c init [--lib] [--name NAME] [--prefix PREFIX] [dir]

| option | spelling | type | default | notes |
|---|---|---|---|---|
| mode | `--emit-c`, `--emit-ir` | bool mode flags | none (one required) | last one wins silently; they are modes, so they are really subcommands spelled as options |
| mode | `fmt` | bare word | | recognised anywhere in argv as a positional-looking word; `m31c x.m31 fmt` works; `lint` and `init` only as the first word |
| output | `-o` | path | stdout (IR/C) | **no long form**; missing value prints usage, exit 2 |
| fmt check | `--check` | bool | off | no short |
| fmt recursive | `-r`, `--recursive` | bool | off | has both, but `-r` collides in meaning with ssh/scp `-r` only superficially (both mean recursive) |
| lint allow | `--allow` | list, comma-separated names | none | rep (extends); no short |
| lint strict | `--strict` | bool | off | |
| lint summary | `--summary` | bool | off | |
| init lib | `--lib` | bool | off | |
| init name | `--name` | str | directory name | |
| init prefix | `--prefix` | str | derived from name | confusable with an install prefix |
| help | `--help` only | | | stdout, exit 0; `-h` is "unknown option"; `lint --help` and `init --help` have their own text |
| version | **none** | | | no `--version`/`-V` (version only in `Cargo.toml`) |

Positionals: `<source>` (path, exactly one, "more than one input file" error), `<dir|file>`, `<file|dir>...` (lint, at least one), `[dir]` (init, default `.`).
Env: none. stdin: unused. stdout: C/IR text (no `-o`), `--help`, lint findings, `init` summary. stderr: diagnostics, usage.
Exit codes: 0 ok; 1 compile error / unformatted (`fmt --check`) / lint findings / file errors; 2 usage error. `lint` returns 2 for an unreadable input but 1 for an unparsable one. **These already match R9.**
Help quality: ok but three different usage dialects; top-level `--help` prints four lines of usage and no option descriptions.

Deviations (D-numbers count toward the table in section 5):
1. `-o` has no long form (R2). 2. `--check`, `--allow`, `--strict`, `--summary`, `--lib`, `--name`, `--prefix` have no shorts (R1; only `-c` for `--check` is worth a short, the rest stay long-only). 3. no `--version`/`-V` (R7). 4. no `-h` (R6). 5. `--emit-c`/`--emit-ir` are modes spelled as options (R8 inconsistency with `fmt`, `lint`, `init` being words). 6. `fmt` accepted anywhere in argv, so `m31c a.m31 fmt` works (grammar ambiguity). 7. no `--` (R4): a file named `-x.m31` cannot be passed. 8. no clustering (R5). 9. no `--opt=value` (R3) in any of the three loops. 10. no closest-match suggestion (R11): `unknown option \`--chek\``. 11. missing value is "usage" with no message for `-o` (R10). 12. repeated `-o`: silently last wins (R12). 13. `--check` with `-r` and a single file: defined by accident. 14. usage text exists in four different shapes (top level, fmt, lint, init) and the top-level `--help` lists no option descriptions (R13).

### 2.2 m31ssh, m31scp, m31sftp, m31ssh-keygen (OpenSSH letters: exempt, long forms to be added)

All four mirror OpenSSH letters and semantics on purpose and are hand-rolled with
an attached-or-next-word value (`-p22`, `-p 22`), clustering of flags (`-tt`, `-qn`),
`--` (scp only), `-V`/`--version`, `-h`/`--help`. None has long forms except
`--help` / `--version`. They are the best-behaved apps today (R3, R4 partly, R5, R6, R7 present).

**m31ssh** (`m31ssh [-46nqTtN] [-e escape|none] [-F config] [-i identity] [-J jump] [-l user] [-L spec] [-o option=value] [-p port] [-R spec] [user@]host [command ...]`)

| letter | meaning | type | rep | notes |
|---|---|---|---|---|
| `-p` | port | int 1..65535 | no (last wins) | error `bad port 'x'` (no range text) |
| `-i` | identity file | path | yes | default `~/.ssh/id_ed25519` |
| `-l` | login user | str | no | |
| `-o` | `opt=val` ssh_config setting | str | yes | first value wins; newline refused |
| `-F` | config file | path | no | default `~/.ssh/config` |
| `-J` | jump host(s) | str | no | comma list |
| `-L`, `-R` | forward spec | str | yes | `[bind:]port:host:hostport` |
| `-e` | escape char or `none` | str(1 char)|none | no | default `~` |
| `-t`, `-tt`, `-T` | tty modes | bool/counter | | `-tt` forces |
| `-N`, `-n`, `-q` | no command, no stdin, quiet | bool | | |
| `-4 -6 -v -a -x -C -k -X -Y` | accepted and ignored | bool | | `-x` is listed twice in the code; `-v` is *ignored* not verbose |
| `-V`/`--version`, `-h`/`--help`/`-?` | | | | version prints `m31ssh 0.1.0 (UNAUDITED)` |

Positional: `[user@]host` (required), then `command ...` (everything after the host is the command, options included, as OpenSSH).
Env: `HOME`, `SSH_AUTH_SOCK` (via lib/ssh). Exit codes: remote command's status, `128+n` for death by signal, **255** for any ssh/usage error (OpenSSH's, kept). Usage errors print `m31ssh: <msg>` + `try 'm31ssh --help'` to stderr, exit 255.
stdin: forwarded to the remote command unless `-n`. Help: good (mentions UNAUDITED, defaults). Missing: long forms, `--` handled but undocumented, no suggestion on unknown option.

**m31scp** (`m31scp [-P port] [-i identity] [-o option=value] [-F config] [-J jump] [-r] [-p] [-q] source ... target`): `-P` port (capital, as scp), `-i -o -F -J` as ssh, `-r` recursive, `-p` preserve mode+times, `-q` no progress, `-C -v -4 -6 -B` accepted/ignored, `-V -h`, `--` ends options. Positional `source ... target` (path or `[user@]host:path`, at least 2). Exit: 0, 1 transfer error AND usage error (OpenSSH scp does the same), 255 connection. Deviations from the convention that are kept on purpose: `-P` (port) vs `-p` (preserve) collision is OpenSSH's.

**m31sftp** (`m31sftp [-P port] [-i identity] [-o option=value] [-F config] [-J jump] [-b batchfile] [-q] [user@]host[:directory]`): `-b` batch file (path; `-` for stdin as OpenSSH), exit 0/1/255. Interactive command language (`ls [-l] [path]`, `cd lcd pwd lpwd get [-r|-R] put [-r] mkdir rmdir rm rename stat chmod help quit`) is OpenSSH's sftp grammar: exempt, unchanged.

**m31ssh-keygen** (`[-t ed25519] [-f file] [-N passphrase] [-C comment] [-a rounds] [-q]`, `-y [-f file] [-P passphrase]`, `-l [-f file]`, `-R host [-f known_hosts]`): `-t` type (only `ed25519`), `-f` file, `-N` new passphrase (str; **secret in argv**, as OpenSSH), `-P` old passphrase, `-C` comment, `-a` KDF rounds int 1..1024 default 16, `-q`, modes `-y` (print public), `-l` (fingerprint), `-R` (remove host from known_hosts). Exit 0 / 1 for every error including usage (OpenSSH uses 1 for usage and 255 for some failures; kept). Mode flags are letters in OpenSSH, so they stay; long forms will be `--print-public`, `--fingerprint`, `--remove-host`.

Deviations (all apps in this group, counted once each): 1. no long forms at all (R2) -- 4 apps. 2. no `--opt=value` (R3). 3. unknown option has no suggestion (R11) -- 4 apps. 4. `-h` missing in OpenSSH but present here (fine, R6 satisfied). 5. m31ssh/sftp/keygen: `--` end-of-options inconsistent (only scp and m31ssh handle it; sftp and keygen treat `--` as an unknown option) (R4). 6. m31ssh and sftp accept `-v` and ignore it, so `-v` does not mean verbose (R7 note). 7. help text of sftp/keygen does not list per-option descriptions (R13). 8. exit code for usage error is 255 (ssh) / 1 (others) not 2 (R9; kept, OpenSSH). 9. `m31ssh -V` prints `m31ssh 0.1.0 (UNAUDITED)` where the compiler is 0.5.0: three separate `VERSION` constants (`SSHC_cli.VERSION`, `SCP_main.VERSION`, ...). 10. repeated singular option: last wins silently for `-p`, `-F`; but first wins for `-o` (R12).

### 2.3 m31git: `1git` (`git.m31`)

Two grammars in one binary.

(a) **Git's own grammar (EXEMPT, drop-in contract in `docs/GIT-COMPAT.md`).** Global prefix `[-C DIR] [-c name=value] [--git-dir DIR] [--work-tree DIR] [--no-pager] [--no-optional-locks] [--no-replace-objects] [-p|--paginate]` then a command word, then that command's git options, each parsed by its own hand-written parser in `core/GIT_git*.m31`. Exit codes are git's (0, 1, 128, 129 for usage). `--help`/`-h` on `log`/`show`/`diff` prints a usage block (stdout, exit 0 for `--help`, 129 for `-h`, as git). **Not touched.**

(b) **Plumbing in `lib/args` grammar: `-cat-file`, `-ls-tree`, `-log`, `-rev-parse`, `-refs`, with `--git-dir`.** This is what the owner called out (`-log --max 5`).

| today | type | default | notes |
|---|---|---|---|
| `--git-dir DIR` (top) | path | `.git` here or above | also in git grammar (exempt there) |
| `-cat-file --type\|--size\|--pretty OBJECT` | bool x3 + positional `OBJECT` (revision) | `--pretty` | |
| `-ls-tree TREE` | positional (tree-ish) | | |
| `-log [--max N] [REV]` | int N, optional rev default `HEAD` | | **shadows git's `log`**, with `--max` where git says `-n`/`--max-count` |
| `-rev-parse REV` | positional | | |
| `-refs` | none | | non-git extra |
| `--help` | stdout, exit 0 | | `-h` is an error naming `--help` (lib/args refusal) |

Several `-commands` may appear on one line and run in order. Exit codes: 0, 2 usage, 128 not a repository.
Env: `GIT_EDITOR GIT_SEQUENCE_EDITOR VISUAL EDITOR GIT_CONFIG_GLOBAL GIT_CONFIG_NOSYSTEM GIT_{AUTHOR,COMMITTER}_{NAME,EMAIL,DATE} GIT_ASKPASS SSH_ASKPASS GIT_TERMINAL_PROMPT GIT_SSL_CAINFO GIT_MERGE_AUTOEDIT GIT_REFLOG_ACTION GIT_TEST_DATE_NOW NO_COLOR TERM COLUMNS HOME XDG_CONFIG_HOME USER LOGNAME EMAIL PWD` (git's) and 1git-only `GITUI_HTTP_USER GITUI_HTTP_PASSWORD GITUI_HTTP_CA_FILE GITUI_SSH_IDENTITY GITUI_SSH_KNOWN_HOSTS GITUI_SYNTAX` (note: still named after the old product, `GITUI_`, not `1GIT_`/`MGIT_`; the rename is in flight in `chore/m31git-rename`).

Deviations: 1. plumbing commands spelled `-word` (R8 vs git's own `log`, which exists beside it: `1git log` and `1git -log` are two different commands). 2. `--max` instead of git's `-n`/`--max-count` (R8). 3. `--type/--size/--pretty` instead of git's `-t/-s/-p` (R1/R8). 4. no `-V`/`--version` (R7). 5. no `-h` in the plumbing grammar (R6). 6. `--git-dir` handled twice, once by `lib/args` and once by the git-grammar prefix loop (two parsers). 7. no `--` (R4) in plumbing (lib/args refuses it). 8. plumbing usage errors exit 2 but git-grammar usage errors 129 (exempt). 9. unknown option suggestion is a prefix-only heuristic (R11). 10. `-refs` has no git counterpart and no `1git`-namespaced home (non-git extras should be `1git refs`).

### 2.4 m31git: `mgit` (`gitui.m31`)

`mgit [path]`: `argv[1]` is the repository path (default `.`). **No options at all.** `mgit --help` tries to open a repository named `--help`.
Env: `NO_COLOR TERM COLORFGBG GITUI_SYNTAX` plus the git ones above (EDITOR launch).
Exit codes: **0 on every failure** (the top level does `return;` after printing `mgit: not a git repository`, `mgit: standard input is not a terminal`); no usage errors exist.
Deviations: 1. no `--help`/`-h` (R6). 2. no `--version`/`-V` (R7). 3. error paths exit 0 (R9, a bug). 4. no `--no-mouse`, `--theme`, `--color` though the TUI honours the terminal (R8, wanted). 5. extra args after the path are ignored silently (R12). 6. `--x` is taken as a path (R11). 7. no `--` (R4).

### 2.5 m3dit: `m3` (`m3.m31`; the repo dir is `m3d1tor`, product `m3`)

    m3 [--no-mouse] [-u KEYMAP|NONE] [FILE...]

| option | spelling | type | default | notes |
|---|---|---|---|---|
| keymap file | `-u FILE`, `-u NONE` | path or the word `NONE` | `$M3_KEYMAP`, else `$XDG_CONFIG_HOME/m3/keymap.vim` | vim's own letter; no long form; a trailing `-u` with no value is silently taken as a FILE named `-u`; `NONE` means no keymap and no state files |
| mouse | `--no-mouse` | bool negation | mouse on, or off if `M3_NO_MOUSE` is non-empty | the only `--no-x` in the code base; there is no positive `--mouse` |
| version | `-v`, `--version` | | | **`-v` is version here** (convention: `-v` verbose, `-V` version) |
| help | `-h`, `--help` | | | text is two lines |
Positional: `FILE...` (path, any count, nonexistent = new buffer). Missing: `+N` / `+cmd` vim startup commands, `--`, `-`.
Env read: `M3_NO_MOUSE M3_KEYMAP M3_CLIP_COPY M3_CLIP_PASTE COLORTERM TERM_PROGRAM TERM COLORFGBG XDG_CONFIG_HOME XDG_STATE_HOME HOME PWD SHELL PATH TMPDIR DISPLAY WAYLAND_DISPLAY`.
stdin/out: needs a terminal on both, else `m3: needs a terminal ...`, exit 1. Exit: 0 on `:q`, 1 any failure; usage errors do not exist (`m3 --bogus` opens a buffer called `--bogus`).
Still-unmerged flags (6.3): `--theme`, `--no-anim`.
Deviations: 1. `-v` means version (R7). 2. `-u` has no long form (R2; vim letter kept, `--keymap` added). 3. `--help`/`--version` scanned over ALL arguments, including filenames after the files (`m3 file -h`), (R4). 4. unknown `--x` is opened as a file (R11). 5. `-u` without value becomes a file (R10). 6. no `--` (R4). 7. no `-V` (R7). 8. no `--mouse` positive form, no `--color` (R8). 9. help is a 2-line string (R13). 10. repeated `-u`: last wins (R12). 11. `--no-mouse` has no short (fine: not a commonly used option).

### 2.6 httpserver (`main.m31`, `lib/args`)

| option | spelling | type | default | notes |
|---|---|---|---|---|
| host | `--host` | str | `0.0.0.0` | |
| port | `--port` | int (checked after parse: 0..65535) | `8080` | range error printed by hand, not by parser; `--port 0` accepted |
| backlog | `--backlog` | int >= 1 | `65535` | |
| tls cert | `--tls-cert` | path | none | must be given with `--tls-key` |
| tls key | `--tls-key` | path | none | |
| handshake timeout | `--tls-handshake-timeout-ms` | int ms >= 1 | `5000` | unit in the name |
| handshake workers | `--tls-handshake-workers` | int >= 1 | `4` | |
| root | `--root` | dir | none (hello mode) | |
| index | `--index` | file name | `index.html` | needs `--root` (checked by hand) |
| listing | `--listing` | bool | off | needs `--root` |
| dotfiles | `--dotfiles` | bool | off | needs `--root` |
| cache | `--cache` | int seconds >= 0 | `0` | needs `--root` |
| help | `--help` | stdout 0 | | |

No positionals. Env: none (README says "no getenv" but `os.env` exists; `PORT` is not read). Exit: 0 (never returns normally), 1 runtime (cannot listen, bad cert), 2 usage and all semantic option errors (good, R9).
Deviations: 1. no shorts: `-p` port, `-r` root are worth having (R1); `--host` stays long-only because `-h` is help. 2. no `-h`, 3. no `-V/--version` (R6/R7). 4. `--tls-handshake-timeout-ms` unit suffix in name (R2/R8: other apps will use `--timeout`; keep but add `--tls-handshake-timeout` taking a duration? kept as is, flagged). 5. option dependencies (`--index` needs `--root`, cert+key together) hand-written, messages not generated (R12). 6. range checks by hand with different wording (`a port is 0..65535`) (R10). 7. no `--` / `--opt=value` is accepted by lib/args but `-p8080` is not (R3). 8. `--listing`/`--dotfiles` have no `--no-x` form and no short (R1). 9. no `--quiet`/`--verbose`, no access log control (R8, none exists). 10. `--cache` unit (seconds) is only in the help text.

### 2.7 term-markdown: `markdown` (`main.m31`, `lib/args`)

| option | spelling | type | default | notes |
|---|---|---|---|---|
| full | `--full` | bool | off | |
| title | `--title` | str | first `#` heading, else file name | |
| theme | `--theme` | enum `default\|bulma\|none` | `default` | enum checked by hand, exit 2 |
| css | `--css` | URL or path | none | URL linked, file inlined |
| output | `--out`, `--o` | path | stdout | **`-o` refused** ("lib/args reads one dash as a command word"); name differs from m31c/ssh-keygen `--output`/`-o` (R8) |
| watch | `--watch` | bool | off | needs `--out` and a file |
| watch limit | `--watch-limit N` | int >= 0 | 0 = none | hidden from `--help` by a pre-pass that strips it before `lib/args` sees the line |
Positional: `[file]` (path, default `-` = stdin). Exit 0, 1 runtime, 2 usage. stdin read when no file. Env: none.
Deviations: 1. `-o` refused, short is `--o` (R1/R3). 2. `--out` vs `--output` (R8). 3. no `-h`, `-V/--version` (R6/R7). 4. hidden `--watch-limit` implemented by a hand pre-pass (R3: accepts `--watch-limit=N` only by a second hand parse). 5. `--theme` has no short; meaning conflicts with tui-markdown's (dark/light/plain vs default/bulma/none) (R8). 6. no `--` (a file named `-x`) (R4). 7. no `--no-full`-style negations (n/a). 8. semantic checks by hand (R10). 9. `--watch` without short (`-w` deserves one) (R1).

### 2.8 tui-markdown (`MDV_cli.m31`, `lib/args`)

| option | spelling | type | default | notes |
|---|---|---|---|---|
| cat | `--cat` | bool | off; implied when stdout is not a terminal | |
| width | `--width N` | int | terminal width, else 80 | a value < 1 is silently replaced by the default (R12 bug) |
| theme | `--theme NAME` | enum `dark\|light\|plain` | from `COLORFGBG`, plain with `NO_COLOR` | **unknown name = warning on stderr, falls back to dark, exit 0** (R10 bug: should be usage error) |
| no-watch | `--no-watch` | bool | watch on | the one `--no-x` here; no positive `--watch` |
Positional tail: `FILE...` (default stdin). Env: `NO_COLOR TERM COLORFGBG`. Exit 0, 1 runtime, 2 usage (including "no file given and stdin is a terminal").
Deviations: 1. no shorts (`-c`? `-w`, `-t`) (R1). 2. no `-h`, `-V/--version` (R6/R7). 3. `--theme` bad value exits 0 (R9/R10). 4. `--width` silent clamp (R12). 5. no `--color auto|always|never` though it has `NO_COLOR` logic, and `--cat` output is ANSI-coloured (R8). 6. `--no-watch` without positive form (R12). 7. no `--` (R4). 8. `--cat` is a flag where `less`/`glow` would take the piped case automatically (design, fine).

### 2.9 tui, syntax, benchmark

* **tui**: no executable. The four examples (`demo browse widgets bench`) read no arguments and no environment. Nothing to change; `scripts/build.sh` takes `[file.m31] [-o out]` and env `M31_ROOT LANGC CC` (no `--help`; R6 for scripts).
* **syntax**: no executable. `tests/oracle/dump.m31` (`LANG PATH [LANG PATH...]`, `-` = detect) and `tests/tools/show.m31` (`SAMPLES`) are developer tools: positional, unchecked (`argv[1]` with no check: an index trap on no arguments, "cannot read" printed with exit 0). `tests/test.sh --bench`. Deviations: 2 (dev tools only, no help, exit 0 on error, R6/R9).
* **benchmark**: `run.sh [-n RUNS] [workload ...]` (only recognised as the *first* argument; `-n` has no long form; `-h`/`--help` run a workload named `--help` and fail with "no workload"; needs `/usr/bin/time` GNU time (not macOS-safe: `-f`), `seq`, `nproc`). Env `M31_ROOT`. Exit 0 / 1. Deviations: 1. `-n` first-only, no `--runs` (R2/R3). 2. no `--help` (R6). 3. unknown option treated as workload (R11). 4. non-portable (macOS) (cross-cutting).

### 2.10 install.sh (identical logic in m31, m31git, m3dit, httpserver, term-markdown, tui-markdown)

Arguments: `--uninstall` only, as `$1`; **any other argument is silently ignored, there is no `--help`**. Env: `VERSION` (release tag), `PREFIX` (default `~/.local`), `LIBC=gnu|musl`. Exit 0 / 1 (`die`). Deviations (per script, 6 scripts): no `--help` (R6), no unknown-argument error (R11), env-only configuration (`VERSION`, `PREFIX`, `LIBC` have no flag equivalents: add `--version`?? -- no: `--version` would be confused with the program version; use `--release TAG`, `--prefix DIR`, `--libc gnu|musl`, `--uninstall`, `-h`) (R2/R8). The env names stay for `curl | sh` use.

## 3. Cross-app: the same concept, spelled differently today

| concept | spellings in use | target |
|---|---|---|
| output file | `m31c -o`; `markdown --out`/`--o`; `build.sh -o`; `m31ssh-keygen` (n/a, `-f`) | `-o, --output PATH` |
| version | `m3 -v/--version`; ssh family `-V/--version`; m31c none; httpserver/markdown/tui-markdown/mgit none | `-V, --version` |
| help | `-h/--help` (ssh family, m3); `--help` only (lib/args apps, m31c); none (mgit, install.sh, run.sh) | `-h, --help` |
| verbose | `-v` ignored (ssh, sftp, scp); `-v` = version (m3) | `-v, --verbose` (counter) |
| quiet | `-q` (ssh family) | `-q, --quiet` |
| theme | `markdown --theme default|bulma|none`; `tui-markdown --theme dark|light|plain` | `--theme NAME`, per-app enum, same meaning (colour scheme); markdown's is a CSS theme: keep values, keep name |
| no mouse | `m3 --no-mouse` (+`M3_NO_MOUSE`) | `--no-mouse` kept; add positive `--mouse` for symmetry |
| no watch | `tui-markdown --no-watch`; `markdown --watch` (opposite polarity default) | `-w, --watch` / `--no-watch` |
| colour | `NO_COLOR` env in mgit, tui-markdown; no flag anywhere | `--color auto|always|never` |
| port | `httpserver --port`; ssh `-p`; scp/sftp `-P` | `-p, --port` (httpserver); ssh letters kept, long `--port` |
| host | `httpserver --host`; ssh positional | `--host` (no short: `-h` is help) |
| config file | `m31ssh -F`; m3 `-u` (keymap) | `-c, --config` for apps (none yet); ssh `-F, --config`; m3 `-u, --keymap` |
| timeout | `httpserver --tls-handshake-timeout-ms` (ms suffix in name) | `--NAME-timeout SECONDS`; keep ms spelling as a deprecated alias |
| list of values | `lint --allow a,b` (comma); `ssh -i` (repeat); `lib/args values` (greedy `--tag a b c`) | repeat the option (`-i a -i b`); comma lists only where the value is itself a list (`--allow`) |
| recursive | `m31c fmt -r/--recursive`; scp `-r` | `-r, --recursive` |
| check/dry-run | `m31c fmt --check` | `--check` (short `-c`, only in `fmt`) |
| input file | positional (all) | positional; `-` = stdin |
| end of options | `--` in scp/ssh; refused by lib/args; absent elsewhere | `--` everywhere |
| exit code usage | 2 (m31c, lib/args apps), 255 (ssh), 1 (scp, sftp, keygen), 129/128 (git) | 2; ssh family and git grammar exempt |

## 4. Shorts that collide or are risky

* `-v`: version in `m3`, ignored in ssh family. Convention: verbose. m3 keeps `-v` as a deprecated alias for `--version` for one minor release.
* `-h`: help everywhere in the new convention, so `--host` gets no short (httpserver).
* `-p`: port (httpserver, ssh), preserve (scp), pager (git `-p`, exempt). Per-app, not shared; scp keeps OpenSSH's.
* `-P`: port in scp/sftp, passphrase in ssh-keygen: OpenSSH's own collision, kept.
* `-r`: recursive in m31c/scp/sftp, `-R` remote forward in ssh: OpenSSH's.
* `-c`: config in the convention, cipher in OpenSSH and `-c name=value` in git. Not used in ssh family or git grammar.
* `-u`: m3 keymap (vim's letter), git `-u`. Kept per app.
* `-o`: output (convention), ssh option (`-o opt=val`): the ssh family keeps OpenSSH's.
* `-n`: dry-run in git, no-stdin in ssh. Per app.
* `-q`, `-V`: consistent already.

## 5. Deviations from Oro's grammar (the count that matters)

| app | deviations | what |
|---|---|---|
| m31c | 6 | bare-word subcommands `fmt lint init`; modes spelled as options (`--emit-c`); single-dash options `-o`, `-r`; no `--version`; `--allow` comma list instead of greedy; hand-rolled parser |
| m31ssh | 7 | OpenSSH single-dash letters as options; attached values (`-p22`) and clustering (`-tt`, `-qn`); `--` ends options; the remote command's dashes are data; no suggestions; hand-rolled parser; ignored letters (`-4 -6 -v ...`) |
| m31scp | 6 | as m31ssh (no remote command) |
| m31sftp | 6 | as m31scp |
| m31ssh-keygen | 7 | letters as options and modes (`-y -l -R`); `-N`/`-P` secrets by letter; as m31ssh |
| 1git | 4 | git's argv grammar is the primary interface (`-C`, `-c`, `-n`, clustered shorts, `--`); plumbing mixes git letters with `-word` commands; no `--version`; two parsers. **Being redesigned by a separate agent** |
| mgit | 3 | no `--help`/`--version`; every failure exits 0; `--x` taken as a path |
| m3 | 5 | vim's `-u` single-dash option; `-v`/`-h` handled by hand, over all arguments; unknown options opened as files; `-u` as the last word taken as a file; hand-rolled parser |
| httpserver | 2 | no `--version`; no `--completions` (it is otherwise Oro-native: `lib/args`) |
| markdown | 2 | as httpserver; hidden `--watch-limit` (Oro: no hidden options) |
| tui-markdown | 3 | as httpserver; `--theme` with a bad name warns and exits 0; a width below 1 silently replaced |
| tui | 0 | no executable |
| syntax dev tools | 3 | positional-only, unchecked `argv[1]`, no `--help`, exit 0 on a read error |
| benchmark run.sh | 3 | `-n` is a single-dash option in first position only; no `--help`; an unknown `-x` runs as a workload |
| install.sh (6 copies) | 3 each | `--uninstall` as an option (a verb: should be `-uninstall`); no `--help`; unknown arguments ignored |

## 6. Exemptions

None. The first pass exempted OpenSSH's letters and git's grammar; the owner struck both.
Developer scripts that are not user-facing (`gates.sh`, `scripts/*.sh`, `build.sh X -o OUT`)
are not covered.

## 7. Resolution (phase 2)

Branches: `feat/cli-args` in m31, `feat/cli-conventions` in each app repo. None is pushed.
Everything that uses `lib/args`' additions waits for m31 0.6.0 before its pin can move.

| app | what changed | old spellings that still work (one release, one warning) |
|---|---|---|
| m31c | `-emit-c`, `-emit-ir`, `-fmt`, `-lint`, `-init`, `--out`, `--check`, `--recursive`, `--version`, `--completions`; several commands per line | `--emit-c`, `--emit-ir`, `fmt`/`lint`/`init` first word, `-o`, `-r` |
| m31ssh, m31scp, m31sftp | clean long names on `lib/args` (`--port`, `--identity`, `--option`, ...); remote command is one operand | the whole OpenSSH spelling, read by the old parser behind one warning |
| m31ssh-keygen | commands `-generate -public -fingerprint -remove` | OpenSSH letters; a bare `m31ssh-keygen` |
| httpserver, markdown, tui-markdown | already Oro-native: gain `--version`, `--completions`, ranges, honest errors | none needed (their spellings did not change) |
| m3 | `--keymap` (vim's `-u` goes; short `--k`), `--version`, `--help`; `--no-mouse`, `--theme`, `--colors`, `--dump-sample` declared (`--no-mouse` is simply a flag with that name: Oro has no negation magic); unknown options are errors | `-u`, `-v` |
| mgit, 1git | **not done here**: a separate agent redesigns the git command surface on `lib/args` | |
| syntax tools, benchmark, install.sh | Oro grammar; `--help`; exit codes | `-n`, `--uninstall` |
