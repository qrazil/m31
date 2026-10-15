# Changelog

Versions follow the git tags of this repository. The dates are the day the
change landed on master.

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
  `net.Conn.fileno`, `net.Listener.fileno`.
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

