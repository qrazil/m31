# Changelog

Versions follow the git tags of this repository. The dates are the day the
change landed on master.

## Unreleased

### Added

- **`const` parameters.** `int total(const List<int> xs)` takes only an
  argument that is already frozen: a `const` local, a module constant, a
  name in an active `const` block, a `const` parameter of the caller, or a
  field or element of any of them. Anything else is a compile error that
  says how to freeze it for the call (`const buf { total(buf); }`). The
  check is static, so the parameter costs nothing at run time; inside the
  function the name is `const`, nothing can be changed through it, and no
  read out of it is held at any depth. `const` is part of the signature a
  method, function or lambda must match an interface by; a spawned
  function's `const` parameter needs a value frozen for good (a module
  constant or a `const` local); a `const` parameter of a type that can
  hold a resource is refused, and so is `const` on a field (reference §4.2;
  docs/const-decision.md, "`const` parameters, as implemented"; corpus
  `1322-const-parameters`, trap `1323`, errors `1324` and `1325`).
- **User-type module constants.** `const Point ORIGIN = Point(0, 0);` at
  the top level: a construction whose arguments are constant expressions is
  one itself, and the instance is laid out as static, immortal data like a
  collection constant -- nested types, collections and maps of them, a type
  that reaches itself, generic types, another module's types (`geo.Point(3,
  4)`), and field defaults, computed in the declaring module. Writes are
  refused where the compiler sees them and trap elsewhere, as for any
  constant. An enum, an interface field and a type with a destructor at any
  depth are refused (reference §4.5; docs/const-decision.md, "User-type
  module constants, as implemented"; corpus `1326`, trap `1327`, modules
  `1328`, errors `765` (now an enum field) and `1329`).
- **`const` types.** `const type Money { .. }`: every instance is frozen,
  deeply, at its construction, by the snapshot a `const` binding takes --
  frozen in place when fresh, deep-copied where a part is held elsewhere.
  Fields may be of any type. A change through any chain that passes through
  an instance, including a bare field in one of its methods, is a compile
  error, and one the compiler cannot see traps; `this` is frozen in its
  methods, reads out of an instance are not held, a value of one passes a
  `const` parameter and `spawn`, and `clone` gives the value itself. A
  `const type` that can hold a resource is refused, and one holding a
  resource behind an interface traps at construction; `const` on an
  interface, enum or distinct type is refused (reference §3.3a;
  docs/const-decision.md, "`const` types, as implemented"; corpus `1330`,
  traps `1331` and `1332`, errors `1333` and `1334`).
- **Recorded: a constant holds no I/O.** A value that owns a resource is
  refused in every permanent form of `const`; inside a `const` block,
  using one traps, as in 0.10.0, by design (docs/const-decision.md,
  "Constants hold no I/O").

## 0.10.0 (2026-10-21)

### Changed

- **A `const` block freezes everything the named value reaches.** It froze
  the named object alone, so inside `const h { .. }` a store into `h.p`
  trapped but one into `h.q.p` -- made through a parameter, where the
  compiler cannot see it -- did not. Now every object reachable from `h` is
  frozen for the block, and on the way out exactly the objects this block
  froze are thawed: anything frozen before it began (a module constant, a
  `const` binding, an enclosing block, a block in a caller) stays frozen.
  The objects a block takes are marked with a new header bit, `RC_HELD`,
  beside `RC_FROZEN`, and listed for the exit; the walk stops at anything
  already frozen, which also ends it on a cycle. It costs a walk of the part
  of the graph not frozen yet at entry and a pass over it at exit; a value
  with no references, such as a `bytes` buffer, still costs a flag set and
  cleared (docs/const-decision.md, "The deep freeze, as implemented";
  reference §4.1a).
- **A `const` binding inside a `const` block copies what the block froze.**
  `rt_snapshot` shared any part that was already frozen, including one only
  a block had frozen, so after `const b { const T c = b; }` the constant
  changed when `b` did. It now shares only parts frozen for good and copies
  the rest, so that binding costs a deep copy of `b`; one that reaches a
  value owning a resource traps, as it does outside a block.
- **Any chain read out of a frozen local or parameter is passed without a
  retain.** 0.9.1 skipped the hold for one step out of a `const` local or a
  name in an active `const` block; with the freeze deep, `h.q.p`,
  `h.ps[i].x` and promoted fields are skipped too. Reads through `this` are
  still held (`frozen_place` in src/middle/lower/hold.rs; corpus
  `1315-const-block-deep-freeze`, traps `1316` to `1321`).

## 0.9.1 (2026-10-20)

### Changed

- **A field or element of a frozen object is passed without a retain.** A
  call argument (or any other operand) read out of a place is held -- an
  `rc_inc` after the load and an `rc_dec` after the operation -- so the callee
  cannot free it by overwriting the place (docs/ir-v0.md §5.1). A place in a
  frozen object cannot be overwritten, since the store traps first, so that
  pair is now skipped for any chain of elements read out of a module constant
  (`T[i][j]`), and for one step -- a field declared on the type, or a List or
  Array element -- out of a local or parameter that an active `const` block
  names or that a `const` declaration bound (`const h { f(h.p, h); }`). Deeper
  reads (`h.q.p`, `h.ps[i]`), promoted fields and fields read through `this`
  are still held: a `const` block freezes the named object alone, and a
  snapshot can share a part only a `const` block froze
  (`frozen_place` in src/middle/lower/hold.rs; corpus
  `1313-frozen-place-argument-is-not-held`, trap
  `1314-frozen-place-argument-overwrite-traps`).

## 0.9.0 (2026-10-19)

Additive; nothing existing changes.

### Added

- `term.Reader.read_keys(timeout_ms, escape_ms)`: `read` with the escape wait
  as its own number. A held incomplete prefix (a lone ESC, half of
  `ESC [ 1 ; 5 A`) is resolved only after `escape_ms` of no further input,
  restarted by every arrival, and `timeout_ms` bounds the call: when it runs
  out first the answer is `Timeout` and the prefix stays held with its
  escape wait still running, so the next call continues it. A loop with a
  ticker passes its tick as `timeout_ms` and a fixed 25 to 50 as
  `escape_ms`: a lone Escape no longer takes 1 to one tick to resolve
  and a sequence split by a slow link no longer comes out as Escape plus characters.
  Zero and negative values are documented on the method. `read` is
  unchanged. `Reader.pending()` already existed and says whether a prefix is
  held. Tested by `runtime/tests/term_keys_wait_test.sh` (a pipe with delayed
  writes), which `gates.sh` runs.

## 0.8.1 (2026-10-19)

### Fixed

- **`if (c) { x = v; } else { x = v; }` never wrote `x`.** The join of an `if`
  decided which variables to carry as block parameters by asking whether the
  two arms disagreed, and two arms that agreed on a new value were treated as
  "unchanged", so `x` came out of the `if` with its old value. It now also
  compares with the value from before the `if`
  (`lower_if`, corpus `1306-if-same-assignment-both-arms`).
- **Assigning a parameter or a `case V(T x)` binding inside a loop or an `if`
  freed memory still in use** (use-after-free, a double free, glibc's
  `malloc(): unaligned tcache chunk detected`). Those names are borrowed, and
  the first assignment took ownership -- registered in the innermost block, so
  the new value was released when that block ended while the name lived on.
  A parameter or payload binding the body assigns is now owned from entry (one
  retain), so every assignment is an ordinary release-old, retain-new. A
  reassigned payload binding is also no longer a slot a `send` could take out
  of the enum (corpus `1307-assign-borrowed-in-nested-block`; found only by
  the sanitizer gate).

## 0.8.0 (2026-10-19)

### Added -- `SHA256SUMS` on every release

The release workflow now publishes a `SHA256SUMS` file beside the platform
tarballs, so an installer can check what it downloads. v0.7.0 has one
uploaded by hand.

### Added

- `sshclient.connect_detailed`: `connect_configured` with the failure as data
  (`ConnectFailure`: `UnknownHost(PendingHost)`, `HostKeyChanged`,
  `HostKeyRevoked`, `Other(str)`), so a program can show a confirm overlay
  instead of parsing a line. `PendingHost` carries the resolved host, port, key
  type, fingerprint and `known_hosts` path and has `record()` for after the
  person said yes. `Detail(is_silent: true)` collects `Warning:` lines instead
  of writing to standard error; `is_quiet` is passed through. Never prompts
  unless `is_interactive`, and never trusts a host key on its own: the
  host-key policy is unchanged, only reported (`docs/ssh-decision.md`,
  "Structured connection failures").
- `sshclient.resolve`: the host, port, user, identity files, `UserKnownHostsFile`
  and `StrictHostKeyChecking` a destination resolves to, after `Include`
  expansion, without connecting.
- `sshconnect.connect_reporting` and `sshconnect.resolve` under them.
  `connect_configured`, `sshconnect.connect`, `Error` and `m31ssh`, `m31scp`,
  `m31sftp` are unchanged.
- `fs.getcwd()` -> `Result<str, io.Error>`: the current directory as an
  absolute path, from a new sys-layer primitive `sys_getcwd` (`getcwd` 79 /
  17 on the raw backend, `getcwd(3)` on libc, which is the only route on
  macOS). A removed current directory is `NotFound`; a path past the
  kernel's 4096 is `Other(36)` on the raw backend.

### Fixed

- `fs.realpath` of a relative path (and `fs.realpath(".")`) no longer reads
  `/proc/self/cwd`: it works on macOS, where it was `Other(38)`, and the
  `Other(38)` fallback is gone. A removed current directory is `NotFound`
  instead of a stale name.

## 0.6.0 (unreleased)

## 0.7.0 (2026-10-18)

### Added -- streamed bodies on the server side

`lib/net/http.m31` can now serve a body larger than memory in both
directions, which a git server, a proxy or an upload endpoint needs:

- **`http.read_request_stream(stream)`** reads and checks a request head
  exactly as `read_request` does and leaves the body on the stream.
  `RequestStream.read(n)` hands it over a piece at a time, through the same
  chunked and Content-Length decoder `BodyStream` uses; there is no ceiling.
  `RequestStream.request()` is the head, `is_finished()` says the stream is at
  the next request.
- **`http.start_response(stream, req, resp, keep_alive)`** writes the head and
  returns a `BodyWriter`: `write(bytes)` sends a piece, `finish()` ends the
  body. Chunked for a 1.1 peer; a caller's own `Content-Length` is enforced in
  both directions (`Framing`, and the connection is marked not reusable);
  nothing is sent for `HEAD`, 1xx, 204 and 304; a 1.0 peer gets a body that
  ends when the connection closes. `BodyWriter.is_keep_alive()` reports the
  outcome.
- `serve` and `Handler` are unchanged and still buffer: a streaming server
  writes its own loop, because only it knows whether an unread body means
  drain or close.

### Changed -- `crypto/sha512` over plain 64-bit words

`lib/crypto/sha512.m31` kept every word as a `Word64` object of two 32-bit
halves, because `>>` used to branch on its operand's sign. `rt_ishr` is
branchless now, so the words are plain `int`s with `wrapping_add`, and
nothing is allocated per block. A 5000-round SHA-512-crypt verify drops from
about 650 ms to about 30 ms; Ed25519, SHA-384 and TLS get the same speedup.
The API is unchanged, except that `digest` now restarts the hasher, so the
next `update` begins a new message (before, a hasher was unusable after
`digest`).

### Fixed

- `BodyStream` (`fetch_stream`, `read_response_stream`) refused a declared
  `Content-Length` above `MAX_BODY`, though it promises no ceiling. It now
  accepts any length.

## 0.6.0

### Changed -- one command-line grammar for everything

Every m31 program now speaks **Oro's `args` grammar**, exactly (oro's
`docs/reference.md`, "`args` -- written in Oro"; `docs/cli-conventions.md`):
one dash is a command (`-serve`, short `-s`), two dashes are an option
(`--out PATH`, `--out=PATH`, short `--o`), anything else is data; several
commands per line; `--help` is the only help (`-h`, `help`, `--h`, a lone `--`
are refused with a message that says what to write); no clustering, no
`--no-x`, no abbreviations (suggested, never accepted).

- **`m31c`** is `m31c -emit-c SRC --out OUT`, `-emit-ir`, `-fmt [--check]
  [--recursive] PATH...`, `-lint PATH... [--allow NAME...]`, `-init [DIR]
  [--lib] [--name N] [--prefix P]`, `--version`, `--completions SHELL`; commands
  may share a line (`m31c -fmt --check lib -lint lib`). **The old spellings
  (`--emit-c`, `--emit-ir`, `fmt`, `lint`, `init` as the first word, `-o`, `-r`) still
  work for this one release and print one deprecation line on stderr each;
  pinned older scripts and CI keep working. They are deleted next release.**
  `--allow` is greedy now: give the paths first.
- **`m31ssh`, `m31scp`, `m31sftp`, `m31ssh-keygen`** drop OpenSSH's letters for
  clean names: `--port`, `--identity`, `--option`, `--config`, `--jump`,
  `--user`, `--local-forward`, `--remote-forward`, `--tty`, `--tty-force`,
  `--tty-never`, `--forward-only`, `--stdin-null`, `--recursive`, `--preserve`,
  `--batch-file`, `--quiet`; `m31ssh-keygen` is four commands, `-generate`,
  `-public`, `-fingerprint`, `-remove HOST`. The remote command of `m31ssh` is
  one operand (`m31ssh host "ls -l"`). **The OpenSSH spellings (`-p 22`, `-tt`,
  `ssh host ls -l`, `ssh-keygen -y -f k`) work for this one release, behind one
  warning, and are then deleted.**
- `install.sh`: `--release TAG`, `--prefix DIR`, `--libc LIBC`, `-uninstall`,
  `--help`, `--version` (the `VERSION`, `PREFIX`, `LIBC` variables still work;
  `--uninstall` still works and warns).
- `m31c -init` scaffolds an app on `lib/args` (`--help`, `--version`,
  `--completions`) and tests it.

### Added

- `lib/args.m31` gains, additively (no form or message of Oro's grammar
  changes): `p.version` (`--version`, answered like `--help`), `p.completions()`
  (`--completions bash|zsh|fish`, generated from the table), `integer(min:, max:)`,
  closest-name suggestions by edit distance, `p.epilog`, `p.parse_or_exit`,
  `p.fail`, `args.migrate` and `args.is_old_style` (old spellings of a released
  program for one release).
- `src/tools/cli.rs`: the same grammar for `m31c`, std only.
- `docs/cli-conventions.md` (the spec, citing Oro's reference) and
  `docs/CLI-INVENTORY.md` (the audit of every m31 app's command line).
## 0.5.1 (2026-10-16)

### Security (fixes from the independent audit of 0.5.0)

- sftp download no longer writes a corrupt file and exits 0 when a server
  returns short reads: gaps are re-read after EOF and a size mismatch fails.
- A server-supplied exit status can no longer abort the client (masked to
  8 bits). Server text is sanitised at every print site; banners are capped
  at 16 KiB and processed in linear time.
- Unsupported or security-relevant `ssh_config` options are reported; `Match`
  criteria and `RevokedHostKeys` that cannot be honoured are refused.
- `m31ssh-keygen -R` keeps `@revoked` and `@cert-authority` lines; known_hosts
  rewrites are atomic; entries of unsupported key types make a host
  unverifiable instead of unknown.
- Rekeying (server- and client-initiated), trailing bytes in the KEX reply
  refused, small-order Ed25519 keys rejected, control characters in user,
  host and key arguments refused, `-r` depth bound, a body-wide read timeout.

### Added

- `~/.ssh/config`: `Include`, `Match`, `HashKnownHosts`, `RekeyLimit`,
  `ProxyCommand`; `m31ssh-keygen -F -H -p`.
- Library: `sshclient.connect_configured` (config, agent, default keys,
  known_hosts policy) for programs that run `git-upload-pack` over ssh.

### Known limits

- No setrlimit, signal handlers, chmod/umask, flock or O_EXCL in the language:
  see `docs/m31ssh.md`.

## 0.5.0 (2026-10-15)

### Added

- **`m31ssh`** (`apps/m31ssh`), an SSH client, **unaudited**
  (`docs/m31ssh.md`):
  - shell over a pty with raw mode and the current size, resize forwarded
    within ~100 ms, `~.` / `~~` / `~?`, command execution with the remote
    exit status (255 on its own errors), large transfers both ways with real
    channel flow control, `-p -i -l -o -F -t -tt -T -n -e -q -J -L -R -N`;
  - Ed25519, ECDSA P-256 and RSA keys, plain or encrypted (bcrypt, four
    ciphers; passphrase prompt with echo off), `ssh-agent` signing for every
    type (signatures verified before use);
  - strict `known_hosts` with a trust-on-first-use prompt
    (`StrictHostKeyChecking` yes/ask/accept-new/no; a changed key is a
    refusal that names the fingerprint and never rewrites the file; the
    client asks for the host-key type the file has);
  - `~/.ssh/config` (`Host` patterns, `HostName`, `User`, `Port`,
    `IdentityFile`, `IdentitiesOnly`, `StrictHostKeyChecking`,
    `UserKnownHostsFile`, `ServerAliveInterval`/`CountMax`,
    `ConnectTimeout`, `ProxyJump`, `LocalForward`, `RemoteForward`,
    `RequestTTY`), keepalives and read timeouts;
  - `ProxyJump` chains, `-L`/`-R`/`-N` with end-to-end flow control.
- Transport: negotiation from an `Offer` (client preference order; nothing
  weaker than what is offered is ever used): `chacha20-poly1305@openssh.com`,
  `aes{256,128}-gcm@openssh.com`, `aes{256,128}-ctr` with
  `hmac-sha2-256(-etm@openssh.com)`; `curve25519-sha256`,
  `ecdh-sha2-nistp256`; host keys `ssh-ed25519`, `ecdsa-sha2-nistp256`,
  `rsa-sha2-512/256`; strict kex (Terrapin). `ssh.Wire` under the
  transport, `ssh.connect_with_offer`/`connect_over` with a `HostKeyCheck`
  callback (one connection, the decision made mid-handshake).
- **`m31scp`, `m31sftp`, `m31ssh-keygen`** (`apps/`): SFTP v3 file copy
  (pipelined, recursive, progress meter), an interactive/batch SFTP client,
  and Ed25519 key generation (plain or bcrypt/aes256-ctr), `-y`, `-l`,
  `-R`; all **unaudited**. `lib/ssh/sshsftp.m31` and `lib/ssh/sshconnect.m31`
  (the connection logic shared by all four programs).
- New modules: `lib/ssh/sshcipher.m31`, `sshagent.m31`, `sshconfig.m31`,
  `sshshell.m31`, `sshjump.m31`, `sshforward.m31`, `sshconnect.m31`, `sshsftp.m31`; `lib/crypto/rsasign.m31`
  (RSASSA-PKCS1-v1_5 signing, CRT, constant-time in the source, self-checked)
  and `lib/crypto/bcryptpbkdf.m31`. `sshkey` reads ECDSA/RSA keys and
  encrypted files (`parse_with_passphrase`, `inspect`); `sshexec` has
  `open_pty`, `shell`, `direct_tcpip`, `window_change`, `write_available`,
  manual window credit and server-open parsing; `sshauth` takes the
  signature algorithm per request (`Signer`, `start`, `try_key`);
  `sshhosts` keeps every key type (`blobs_for`, `algorithms_for`);
  `net.Conn.fileno`, `net.Listener.fileno`, `net.connect_polled` (connect with `poll(2)` and a timeout), `ssh.PollWire` and `Transport.recv_once`.
- `tests/ssh`: `test_m31ssh.sh` (150 pty-driven checks against disposable
  `sshd`s, including a corrupting proxy), `test_m31sftp.sh` (56), and oracles `oracle_sshcipher.py`, `oracle_sshkeyfiles.py`,
  `oracle_rsasign.py`, the bcrypt_pbkdf vector, plus unit tests for config,
  escape filter, message layouts, negotiation, host-key blobs, agent
  parsers and forwarding routes.

### Changed

- `ssh.KexInit`'s own proposal is no longer a single suite (the library's
  v0 rule, `docs/ssh-decision.md`): see "Transport" above. `connect_any`
  and `presented_host_key` still work as before for ed25519 host keys.
- `known_hosts` lines of `ecdsa-sha2-nistp256` and `ssh-rsa` are parsed and
  validated (a malformed one is an error, never a silent skip); other key
  types are still skipped.
- `sshauth.Signer.sign_blob` takes the algorithm; `sshkey.Error` has
  `WrongPassphrase`, `UnsupportedCipher`, `UnsupportedKdf`; `ssh.Error` has
  `BadKeyExchange`; `sshexec.Error` has `PtyRefused`.

## 0.4.6 (2026-10-15)

### Added

- Alpine Linux / musl libc, x86-64 and aarch64: every gate runs on Alpine in
  CI (`.github/workflows/alpine.yml`, `scripts/ci-alpine.sh`), and a release
  carries `m31-linux-x86_64-musl.tar.gz` and `m31-linux-aarch64-musl.tar.gz`
  next to the glibc ones, same layout. The compiler in them is a static
  executable (it runs on glibc systems too); programs are built by the
  system `cc`, so Alpine needs `gcc musl-dev linux-headers`. `install.sh`
  picks the musl asset on a musl system and falls back to the glibc asset
  for a release that has none.

## 0.4.5 (2026-10-13)

### Fixed

- A child process starts with SIGINT, SIGQUIT, SIGHUP and SIGTERM at their
  defaults, as it already did SIGPIPE, even when the program that started
  this one ignored them (a CI runner's job shell ignores SIGINT, which made
  `^C` on a pty a no-op). 0.4.4 was never tagged: its changes ship in 0.4.5.
- `term.Decoder` is linear in its input. An unfinished bracketed paste was
  searched for its end from the first byte on every feed, so a 1 MB paste in
  4 KB reads took half a second (and 16 MB would have taken minutes); it now
  looks only at the bytes that arrived since. A large feed of ordinary keys
  no longer moves the rest of the buffer once per event (300000 keys in one
  feed: 2 s down to under 0.1 s). Events are unchanged byte for byte.
  `corpus/modules/term-decoder-feeds` feeds pastes in many chunk sizes,
  splits paste markers and keys at every offset, and carries generous
  timing guards.

## 0.4.4 (2026-10-13)

### Fixed

- A child process now gets only descriptors 0, 1 and 2 even when this process
  was itself started with others open and not close-on-exec (a CI runner's, a
  shell's `exec 9<file`): the raw backend marks everything above 2
  close-on-exec before the exec (`close_range`), the libc backend closes it
  with posix_spawn's closefrom action (glibc 2.34+) or macOS's
  `POSIX_SPAWN_CLOEXEC_DEFAULT`. 0.4.3's CI caught the leak that a clean
  development shell never shows.
- `proc.start_pty` on macOS: the window size is set again once the child has
  the slave open (the kernel forgets a size set before the first open), and
  through the slave when it refuses it on the master.
- Internal: the unit tests that run `git` or change `PATH` share one lock.

## 0.4.3 (2026-10-12)

### Added

- `proc` (`lib/proc.m31`, `docs/proc-decision.md`): child processes with their
  standard streams on pipes. `proc.run(argv, input:, cwd:, env:, timeout_ms:,
  should_check:, is_quiet:)` returns a `Completed` (status, returncode, output
  and error, capped at 64 MiB each with `is_truncated`); `proc.start(argv,
  cwd:, env:)` returns a `Proc` whose `stdin`, `stdout` and `stderr` are
  `proc.Pipe`s -- ordinary `io` streams that park only the green thread
  through the reactor and hold no unbounded buffer -- with `wait()`,
  `kill(signal)` and a destructor that kills and reaps. `argv` is a list of
  strings straight to `execve`; `env` replaces the environment. Errors are the
  payload-free `proc.Error`. `os.run` is unchanged. (`proc.start`, not
  Oro's `spawn`: `spawn` is a keyword.)
- Sys layer: `sys_pipe`, `sys_proc_start_fds` (child stdio by descriptor,
  working directory, SIGPIPE and signal mask reset), `sys_kill`,
  `sys_readlink`, on both backends, with `rt_*` wrappers. A child's `$PATH`
  lookup now uses the `PATH` of the environment it is given on both backends.
- `proc.start_pty(argv, rows:, columns:, cwd:, env:)` returns a `Pty`: the
  child on a pseudo-terminal in a session of its own, `master` one readable
  and writable stream, `resize(rows, columns)`, `wait()`, `kill(signal)`.
  Sys layer: `sys_pty_open`, `sys_set_winsize`, `sys_proc_start_pty`. Tested
  against Python's `pty` module (`tty`, `stty size`, an interactive `cat`, a
  raw-mode program, `^C`, a resize) and in corpus `proc-pty`.
- `timer.monotonic_ns()`: nanoseconds on the monotonic clock.
- `fs.readlink(path)` and `fs.realpath(path)` (Python's
  `os.path.realpath(strict=True)`; relative paths need `/proc/self/cwd`).
- Tests: corpus `proc-run`, `proc-start`, `proc-many`; `tests/proc/test_proc.sh`
  (parity with Python's `subprocess`, 200 MiB through `cat` in constant
  memory, 8 MiB of stdin beside draining, the cap, timeouts, EPIPE, on both
  backends) and `tests/fs/test_realpath.sh`, both in `gates.sh` and in the
  macOS and aarch64 workflows.

### Fixed

- The reactor's epoll descriptor and its two eventfds were not close-on-exec
  and reached every child process; they are now (and the kqueue descriptor).

## 0.4.2 (2026-10-12)
### Fixed

- `http.read_request` no longer traps on a request target that is not UTF-8:
  the target decoded to the empty string and was then indexed. It is now
  refused as a `Syntax` error (status 400). Tests: the corpus entry
  `stdlib-http-invalid-utf8` and `tests/http/test_invalid_utf8.sh`, which sends
  raw invalid-UTF-8 requests from Python's `socket` to a live server.

### Added

- `io.File.seek(offset)`, `seek_by(delta)`, `seek_from_end(offset)` and
  `tell()`: reposition and query a file, accounting for the 64 KiB read-ahead
  (so `seek_by` and `tell` are relative to what `read` would hand out next).
  Tests: corpus `stdlib-io-seek`, and `tests/io/test_seek.sh`, which compares
  random operation scripts across the read buffer with Python's raw file
  object.

- `http.fetch_stream(request, timeout_ms:)` and `http.read_response_stream(stream,
  method)` return a `BodyStream`: the head is read, the body comes out of
  `read(n)` as it arrives (Content-Length, chunked or close-delimited), with
  no ceiling and no buffering. A body that stops short of its framing is an
  error. `fetch`, `get` and `read_response` are unchanged. Tests: corpus
  `stdlib-http-stream` and `tests/http/test_stream.sh`, where a Python origin
  server withholds the second half of every body until the client has read
  the first, so a buffering client deadlocks.

- ssh read timeouts: `ssh.connect` / `ssh.connect_any` and
  `sshclient.connect` / `connect_with` take `timeout_ms` (default 0, wait for
  ever), `ssh.Transport.set_read_timeout(ms)` and
  `sshclient.Session.set_read_timeout(ms)` change it later. They bound every
  wait for the server -- handshake, authentication and each channel read --
  and a wait that runs out is the new `ssh.Error.TimedOut` (before, a timeout
  was folded into `Io`). After `TimedOut` the transport must be closed. This
  adds a variant to `ssh.Error`: an exhaustive `match` over it needs a
  `case TimedOut`. Tests: `tests/ssh/test_timeout.sh`, against a real `sshd`
  and a silent Python peer, timed from the shell.

- Transitive dependencies: a dependency's own `deps` is followed, for path and
  git dependencies alike, recursively. Paths in it are relative to that
  dependency's root; a diamond is resolved once; two different sources for one
  name, and dependency cycles, are errors that name the chains. Every git
  dependency of the graph is locked in the top project's `deps.lock` and cached
  flat in its `.m31-deps/`. See `docs/project-layout-decision.md`.

### Changed

- The path-dependency graph is checked when the project is loaded, so a cycle
  or conflict is reported even if nothing imports the branch it is in.
- A one-file dependency (`import x;` resolved through `deps`) now resolves its
  own imports against that dependency's root rather than the importer's, and
  importing it from several modules no longer reports a collision.
- Generated C for existing programs is unchanged.


## 0.4.1 (2026-10-12)

Repository layout, plus the new `m31c init`; no language or standard-library
behaviour changes.

- Root scripts moved to `scripts/`: `rename.sh`, `run.sh`, `sanitize.sh` and
  `bench/run.sh` (now `scripts/bench.sh`). `build.sh`, `config.sh` and
  `gates.sh` stay at the root.
- `runtime/tests/` holds the runtime's own test suites and demos and
  `runtime/tools/with_timeout.sh` the helper; the library files and `arch.sh`
  stay in `runtime/`. The release tarball no longer contains `runtime/tests/`.
- `apps/ssh`, `apps/tls` and `crypto25519_test/` are now `tests/ssh`,
  `tests/tls` and `tests/crypto25519`; `apps/` is gone. `m31c lint` covers
  `tests/` instead.
- `docs/README.md` indexes the documentation; the stray `ts.exp` was removed.

### Added

- `m31c init [--lib] [--name N] [--prefix PFX] [dir]` creates a new project in
  the standard layout, with a passing test, CI and release workflows.
  `docs/project-layout.md` describes the layout and the rules behind it.

### Changed (source layout; no change to the language or the generated C)

- `src/` is grouped into `front/` (lexer, parser, ast, diag), `resolve/`
  (modules, deps, stdlib), `middle/` (mono, lower, hoist, width, ir), `back/`
  (emit_c) and `tools/` (fmt, lint, init).
- The largest files are split by area: `src/tests.rs` into `src/tests/`,
  `parser` into `types`, `decl`, `stmt`, `expr`; `emit_c` into `types`,
  `statics`, `func`, `inst`, `blocking`; `lower/expr` into `call`, `enums`,
  `literal`, `ops`; `mono` into `types`, `items`, `body`, `infer`. The emitted
  C of the whole corpus, lib, examples and apps is byte-identical before and
  after.
- The blocking-FFI warning's text names `src/back/emit_c/blocking.rs` instead
  of `src/emit_c.rs`.

## 0.4.0 (2026-10-10)

0.4.0 is a breaking release in two ways, both mechanical to follow: the
standard library is grouped into folders, and every public name that broke the
naming rules of `docs/naming-decision.md` was renamed. `m31c lint` is now a
gate: `lib/`, `apps/`, `examples/`, `runtime/`, `bench/` and
`crypto25519_test/` have no findings (2495 before, 1955 of them in `lib/`).

Programs that import the standard library and pin `M31_REF` to an older tag are
unaffected until they move to 0.4.0.

### Standard library folders

Imports of a grouped module now name the group: `import crypto.sha256;`. The
module is still used by its last segment alone (`sha256.hex(..)`); a grouped
module imported flat (`import sha256;`) is an error that says what to write.
`docs/stdlib-layout-decision.md` has the rule and the reasons.

| Group | Modules |
|---|---|
| `crypto` | aes aesgcm bignum chacha20poly1305 consttime ecdsa ecdsasign ed25519 field25519 hkdf hmac rsa scalar25519 sha1digest sha256 sha512 x25519 |
| `encoding` | base64 csv der hexcodec html json |
| `net` | deadlinestream http https |
| `pki` | clientcert ocsp signingkey x509 x509chain |
| `ssh` | sshauth sshclient sshexec sshhosts sshkey |
| `text` | diff regex unicode |
| `tls` | tls12 tls13record tls13schedule tlsresume tlsserver |

Not moved: `args date fs io math net os random sort term text timer tls ssh`.
Compiler: the embedded table in `src/stdlib.rs` is keyed by the dotted path and
`src/modules.rs` resolves and diagnoses group imports.

### Renamed: public module functions

Call sites change; arguments do not. Private functions renamed the same way are not listed.

| Module | Old | New |
|---|---|---|
| `ecdsa` | `verify_der` | `is_valid_der_signature` |
| `ecdsa` | `verify_raw` | `is_valid_raw_signature` |
| `ed25519` | `verify` | `is_valid_signature` |
| `field25519` | `sq` | `square` |
| `rsa` | `verify_pkcs1_v15` | `is_valid_pkcs1_v15_signature` |
| `rsa` | `verify_pss` | `is_valid_pss_signature` |
| `fs` | `exists` | `is_present` |
| `fs` | `is_dir` | `is_directory` |
| `http` | `serve_conn` | `serve_connection` |
| `os` | `args` | `arguments` |
| `os` | `args_bytes` | `arguments_bytes` |
| `x509` | `same_name` | `is_same_name` |
| `term` | `fg` | `foreground` |
| `term` | `bg` | `background` |
| `tls12` | `signed_params` | `signed_parameters` |

### Renamed: constants

| Module | Old | New |
|---|---|---|
| `ed25519` | `D` | `EDWARDS_D` |
| `sha256` | `K` | `ROUND_CONSTANTS` |
| `http` | `LF` | `LINE_FEED` |
| `http` | `CR` | `CARRIAGE_RETURN` |
| `http` | `SP` | `SPACE` |
| `der` | `TAG_OID` | `TAG_OBJECT_ID` |
| `clientcert` | `OID_*` (1 constants) | `OBJECT_ID_*` |
| `ocsp` | `OID_*` (6 constants) | `OBJECT_ID_*` |
| `signingkey` | `OID_*` (1 constants) | `OBJECT_ID_*` |
| `x509` | `OID_*` (13 constants) | `OBJECT_ID_*` |
| `x509chain` | `OID_*` (7 constants) | `OBJECT_ID_*` |
| `sshauth` | `SSH_MSG_*` (5 constants) | `SSH_MESSAGE_*` |
| `sshexec` | `SSH_MSG_*` (13 constants) | `SSH_MESSAGE_*` |
| `ssh` | `SSH_MSG_*` (10 constants) | `SSH_MESSAGE_*` |

### Renamed: fields and methods

A field or method is renamed on the type that declares it. Named arguments of
a struct constructor follow the field.

| Type | Old -> New |
|---|---|
| `args.Clause` | toks -> tokens |
| `args.Parsed` | given -> is_given |
| `args.Parser` | pos -> position |
| `csv.Scan` | at -> position |
| `der.Element` | oid -> object_id |
| `ecdsa.Point` | x -> x_coordinate, y -> y_coordinate, z -> z_coordinate |
| `ecdsa.PublicKey` | x -> x_coordinate, y -> y_coordinate |
| `ecdsa.Signature` | r -> r_component, s -> s_component |
| `ecdsasign.Point` | x -> x_coordinate, y -> y_coordinate, z -> z_coordinate |
| `ed25519.Point` | t -> t_coordinate, x -> x_coordinate, y -> y_coordinate, z -> z_coordinate |
| `fs.Stat` | is_dir -> is_directory |
| `http.Headers` | ns -> names, vs -> values |
| `http.Request` | framed -> is_framed |
| `http.Response` | framed -> is_framed |
| `io.Buffer` | at -> position, buf -> buffer, live -> is_live |
| `io.File` | at -> position, buf -> buffer, fd -> descriptor, live -> is_live, readable -> is_readable, writable -> is_writable |
| `json.Frame` | object -> is_object |
| `json.Job` | at -> position, object -> is_object |
| `json.Parser` | at -> position |
| `net.Addr` | ip -> ip_address, num -> number |
| `net.Conn` | at -> position, buf -> buffer, fd -> descriptor, live -> is_live |
| `net.Listener` | fd -> descriptor, live -> is_live |
| `os.ExitStatus` | success -> is_success |
| `regex.ClassDef` | hi -> high, lo -> low, negate -> is_negated |
| `regex.Parser` | eof -> is_at_end, pat -> pattern, pos -> position |
| `regex.Thread` | pc -> program_counter |
| `sha1digest.Hasher` | h0 -> state0, h1 -> state1, h2 -> state2, h3 -> state3, h4 -> state4 |
| `sha256.Hasher` | h0 -> state0, h1 -> state1, h2 -> state2, h3 -> state3, h4 -> state4, h5 -> state5, h6 -> state6, h7 -> state7 |
| `sha512.Hasher` | h -> state, k -> round_constants |
| `sha512.Word64` | hi -> high, lo -> low |
| `ssh.KexInit` | first_kex_packet_follows -> is_first_kex_packet_following |
| `ssh.Transport` | conn -> connection |
| `sshauth.Failure` | partial_success -> is_partial_success |
| `sshexec.Channel` | err_buf -> error_buffer, got_close -> has_received_close, got_eof -> has_received_eof, out_buf -> output_buffer, read_all_err -> read_all_error, read_err -> read_error, sent_close -> has_sent_close, sent_eof -> has_sent_eof |
| `sshexec.Request` | want_reply -> should_reply |
| `sshhosts.Entry` | revoked -> is_revoked |
| `term.Csi` | params -> parameters |
| `term.Decoder` | buf -> buffer |
| `term.Reader` | buf -> buffer, fd -> descriptor |
| `term.Session` | live -> is_live |
| `term.Size` | cols -> columns |
| `term.Step` | ev -> event |
| `term.Writer` | buf -> buffer, f -> file |
| `tls.Conn` | at_end -> is_at_end, closed -> is_closed, failed -> is_failed, fixed_clock -> is_fixed_clock, resumed -> is_resumed, stapled -> is_stapled |
| `tls.Outcome` | resumed -> is_resumed, ticketable -> is_ticketable |
| `tls.Reader` | at -> position, u8 -> octet |
| `tls.ServerChoice` | legacy -> is_legacy, psk_selected -> is_psk_selected, status_promised -> is_status_promised |
| `tls12.Suite` | uses_sha384 -> is_sha384 |
| `tls13record.Connection` | close_sent -> has_sent_close, failed -> is_failed, legacy -> is_legacy, peer_closed -> is_peer_closed, peer_finished -> is_peer_finished, read_protected -> is_read_protected, received_close_notify -> has_received_close_notify, sent_close_notify -> has_sent_close_notify, write_protected -> is_write_protected |
| `tls13schedule.Suite` | uses_sha384 -> is_sha384 |
| `tls13schedule.TrafficKeys` | iv -> initialization_vector |
| `tlsresume.SessionCache` | enabled -> is_enabled |
| `tlsserver.Config` | serves -> can_serve |
| `tlsserver.Conn` | at_end -> is_at_end, closed -> is_closed, failed -> is_failed |
| `tlsserver.Hello` | offers_tls13 -> is_tls13_offered, offers_x25519 -> is_x25519_offered |
| `tlsserver.Reader` | at -> position, u8 -> octet |
| `x509.AlgorithmIdentifier` | algorithm_oid -> algorithm_object_id |
| `x509.Certificate` | allows_server_auth -> is_server_auth_allowed, issued_by -> is_issued_by, matches_hostname -> is_valid_for_hostname, must_staple -> needs_staple, unhandled_critical -> has_unhandled_critical |

Methods renamed from an ambiguous short name, by type: `http.Headers.has` is
`has_name`, `term.Key.has` is `has_modifiers`, `tls.HelloExtensions.has` is
`has_extension` (a private type), and `args.Parsed.flag` is `is_flag_set`.

### Renamed: named arguments

A named argument is part of the call, so these change at every call that
writes them.

| Function | Old | New |
|---|---|---|
| `base64.encode` | `url:` | `is_url_safe:` |
| `base64.decode` | `url:` | `is_url_safe:` |
| `http.unquote` | `plus:` | `should_decode_plus:` |
| `x509.parse_pem` | `trust_anchors:` | `is_trust_bundle:` |
| `term.reader` | `fd:` | `descriptor:` |
| `term.size` | `fd:` | `descriptor:` |

### Renamed: positional parameters, locals

Parameters are positional, so renaming one changes no call. About 2150
parameters, locals, loop variables and `case` bindings were renamed in the
standard library, the apps, the examples and the runtime tests; unused `case`
payloads became `_`. They are not listed here.

The v0.3.3 additions follow the same rules: `sshhosts.add` takes `is_hashed`
and `ssh.presented_host_key` / `ssh.HostKey` are unchanged in name.

### Changed behaviour

- `os.arguments()` traps with `os.arguments(): argument N is not valid UTF-8;
  read os.arguments_bytes() instead` (the message names the new functions).
- `apps/tls/tls13_hex.m31` is `apps/tls/TLS13_hex.m31`, and its trap messages
  start `TLS13_hex:`.
- The kqueue reactor reports a descriptor Darwin will not watch (`/dev/null`, regular files) as `EPERM`, as epoll does, so terminal reads on a non-tty park only the green thread on macOS too.

### Gates

- `gates.sh` runs `m31c lint lib apps examples runtime bench crypto25519_test`
  after the stdlib formatting gate. `corpus/` is exempt, as the lint's own
  rules say.

### Moving a program from 0.3.x

1. Rewrite imports of the grouped modules (`import sha256;` becomes
   `import crypto.sha256;`).
2. Rename the functions, constants, fields, methods and named arguments in the
   tables above. The compiler names each one that is missing.
3. Nothing else changes: the language and the runtime are the same.

