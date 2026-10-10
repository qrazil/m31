# Changelog

Versions follow the git tags of this repository. The dates are the day the
change landed on master.

## 0.4.0 (unreleased)

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

### Changed behaviour

- `os.arguments()` traps with `os.arguments(): argument N is not valid UTF-8;
  read os.arguments_bytes() instead` (the message names the new functions).
- `apps/tls/tls13_hex.m31` is `apps/tls/TLS13_hex.m31`, and its trap messages
  start `TLS13_hex:`.

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

