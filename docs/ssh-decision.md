# SSH: the decision

Status: **Phases 1, 2, 3 and 4 implemented and validated.**
Phase 1 (crypto primitives) is `lib/crypto/sha256.m31`, `lib/crypto/chacha20poly1305.m31`,
`lib/crypto/x25519.m31`, `lib/crypto/ed25519.m31` (plus `lib/crypto/field25519.m31`/
`lib/crypto/sha512.m31`/`lib/crypto/scalar25519.m31` underneath), validated against RFC
test vectors and real oracles in `crypto25519_test/test.sh` and
`apps/ssh/test.sh`. Phase 2 (the transport protocol) is `lib/ssh.m31`,
validated two ways: `apps/ssh/test.sh`'s pure wire-format checks (no
`sshd`), and `apps/ssh/test_transport.sh`'s live handshake against a real,
disposable local `sshd` — including a capstone round trip
(`SSH_MSG_SERVICE_REQUEST`/`SSH_MSG_SERVICE_ACCEPT`) proving the derived
keys actually decrypt real traffic, not just that no error was raised.
Phases 3 and 4 are `lib/ssh/sshkey.m31` (unencrypted `openssh-key-v1` Ed25519
keys; encrypted ones are refused with a clear error), `lib/ssh/sshhosts.m31`
(`known_hosts`, plain, `[host]:port` and hashed; no trust on first use),
`lib/ssh/sshauth.m31` (RFC 4252 `publickey`), `lib/ssh/sshexec.m31` (RFC 4254
session channel: `exec`, flow control, stdin, separate stdout/stderr,
exit status) and `lib/ssh/sshclient.m31` (all of it in one `connect` call).
`ssh.connect_any` takes a list of acceptable host keys, and `ssh.Link`
(`send`/`recv`/`session_id`) is what the new layers run over, so they are
tested against scripted in-memory peers (`apps/ssh/t_ssh_auth.m31`,
`t_ssh_exec.m31`, `t_ssh_key.m31`, `t_ssh_hosts.m31`, run by
`apps/ssh/test.sh`) and, live, against a disposable `sshd` with fresh keys
(`apps/ssh/test_auth_exec.sh`: wrong key, unknown user and unknown host
refused; exit status; stderr separation; 8 MB read and write; a 6 MB `cat`
round trip). A caller that wants to ask a person about an unknown host
(trust on first use, as `ssh` does) has `ssh.presented_host_key` and
`sshhosts.add`; see "Host keys and trust on first use" below. Known limits: Ed25519 keys only, no rekeying, no
passphrase-protected keys, and the receive window is replenished as data
arrives (an unread stream is buffered in memory, not back-pressured). This
exists because
`apps/git/design.md` explicitly held SSH back from the packfile/smart-HTTP
work and required its own pass before any code gets written — a bug in a
cryptographic transport is a vulnerability, not a wrong diff, and that
changes how this has to be built, tested, and reviewed compared to
everything else in this codebase so far.

**This is a library first.** It lands in `lib/`, gets validated entirely on
its own — against a real OpenSSH server, in disposable fixtures, the same
discipline as everything else — and is not wired into `apps/git` until that
validation is done. `apps/git` is a *consumer* of this library, not where
it's developed.

---

## 1. Scope: what this needs to do, and no more

The only thing `apps/git` actually needs from SSH is: connect, authenticate,
run one remote command (`git-upload-pack` or `git-receive-pack`), and get a
bidirectional byte stream for the duration. Everything after that byte
stream exists is git's own wire protocol, already implemented
transport-independently in `apps/git/httpfetch.m31`'s pkt-line/negotiation
logic — SSH does not need to know anything about git.

So the v0 library scope is a **client**, not a server, with exactly enough
of the SSH connection protocol to open **one `session` channel and `exec`
one command** — no PTY allocation, no shell, no port forwarding, no SFTP
subsystem, no multiple channels. Authentication is **publickey only** —
this is what every git host (GitHub, GitLab, Gitea/orogit) expects for
machine access, and it sidesteps interactive password/keyboard-interactive
prompting entirely, which is a real UI problem this library does not need
to solve to be useful.

**Explicitly out of scope for v0, each a named, separate gap, not an
oversight:**
- SSH server side (nothing here needs to *accept* SSH connections)
- Password / keyboard-interactive auth
- PTY / shell / port-forwarding / SFTP channel types
- Host-key management UX (the prompt itself) — the library takes the
  expected host keys as a parameter and refuses to proceed if the server's
  is not among them; where that expectation comes from, and whether a
  person is asked, is the caller's job. `lib/ssh/sshhosts.m31` reads and appends
  `known_hosts`, and `ssh.presented_host_key` gives the caller what to ask
  about (below)
- Algorithm negotiation beyond the one modern suite below — no fallback to
  older algorithms, ever (see §2)
- Rekeying (RFC 4253 §9's periodic re-key) — v0 fails a connection that
  runs long enough to need it rather than implementing it; git's own
  clone/fetch/push operations are not long-lived enough for this to matter
  in practice, and it is a real, named follow-up if that assumption breaks

## 2. Algorithm choice: one modern suite, no fallback

**curve25519-sha256 (key exchange) + ssh-ed25519 (host key and user auth
key type) + chacha20-poly1305@openssh.com (cipher, and it provides its own
MAC — no separate HMAC needed).** This is deliberately the smallest
correct modern surface, not the most compatible one, and the reasons are
specific to writing this from scratch with no dependencies and no
`unsafe`:

- **No bignum arithmetic.** RSA (the older default) needs arbitrary-precision
  modular exponentiation, which is exactly the kind of code where a subtle
  bug becomes a timing side-channel. X25519 and Ed25519 use fixed-size,
  well-documented field arithmetic over a single specific prime
  (2²⁵⁵ − 19), with reference implementations designed from the start to be
  constant-time (the Montgomery ladder for X25519 has no secret-dependent
  branches by construction).
- **No block cipher, no CBC/CTR mode reasoning.** ChaCha20 is a simple
  ARX (add-rotate-xor) stream cipher — far less machinery than AES's S-boxes
  and key schedule, and no mode-of-operation padding/IV-reuse footguns to
  get wrong.
- **Every primitive has official test vectors** to build against before any
  live interop is attempted: RFC 7748 (X25519), RFC 8032 (Ed25519), RFC 8439
  (ChaCha20-Poly1305), FIPS 180-4 (SHA-256, needed for the key-exchange
  hash and key derivation).
- **This is also what a modern OpenSSH client prefers by default** — not a
  niche choice, the common case for any reasonably current server.

**No algorithm fallback, ever, in v0.** If a server doesn't offer this exact
suite, the connection fails with a clear error. A fallback path multiplies
the amount of cryptographic code that has to be correct for a marginal
compatibility gain against hosts (GitHub, GitLab, any current Gitea) that
all support this suite today.

## 3. Phases

**Phase 1 — crypto primitives, in isolation.** X25519, Ed25519, ChaCha20,
Poly1305, SHA-256. Each one is pure, self-contained, and validated against
its RFC's official test vectors before it touches any networking code at
all. This is the safest, most mechanical phase — no protocol state machine,
no live server, just "does this function produce the vector's expected
output for the vector's input." Lands in `lib/` as small, focused modules.

**Phase 2 — transport protocol.** Version exchange, the binary packet
protocol (length-prefixed, padded, MAC'd), `KEXINIT` negotiation (always
proposing exactly the Phase-1 suite), the key exchange itself, and deriving
the session's encryption/MAC keys from the exchange hash. Validated against
a real, disposable local OpenSSH server (`sshd` in a throwaway container or
chroot-equivalent, generated host key, torn down after the test) — the
oracle is "does a real `sshd` accept this handshake and derive the same
session keys," not just "does our own code agree with itself."

**Phase 3 — publickey authentication.** The `ssh-userauth` service,
`publickey` method, against the same disposable `sshd` fixture with a
known test key authorized for it.

**Phase 4 — connection protocol, minimal.** Open one `session` channel,
send one `exec` request, get back a working bidirectional byte stream
(stdout/stderr demultiplexing, channel close/EOF handling). This is the
handoff point to `apps/git` — once this phase is done, `git-upload-pack`
over SSH is "point `httpfetch.m31`'s existing protocol logic at this
stream instead of an HTTP body," not new git-protocol work.

**Only after all four phases are independently validated does `apps/git`
gain an SSH remote path.** That integration is its own follow-up, scoped
separately, per `design.md`'s existing note.

## 4. Testing discipline

Same rule as everything else in this codebase, sharpened for the stakes:
**every test runs against a disposable fixture, and the oracle is a real
implementation, not this project's own code checked against itself.**
Concretely: a local, throwaway `sshd` instance with a generated host key
and one authorized test key, spun up and torn down per test — never a real
server, never this project's own deployed `orogit`. Where a test vector
exists (all of Phase 1), it is the primary oracle; where one doesn't
(Phases 2–4's live protocol behavior), a real OpenSSH client or server is.

## Open

- ~~Where the expected host key comes from in `apps/git`'s eventual usage~~
  — decided: a `known_hosts` file (`lib/ssh/sshhosts.m31`), with the caller
  prompting for unknown hosts (below)
- Whether Phase 1's primitives get exposed as their own general-purpose
  `lib/crypto`-style module (useful beyond SSH) or stay private to the SSH
  library — leaning toward exposing them, since a from-scratch, tested
  X25519/Ed25519/ChaCha20-Poly1305/SHA-256 set is broadly useful and this
  project has no crypto primitives at all today, but not deciding it before
  Phase 1 exists
- Rekeying (§1) if a real usage pattern ever needs it

## Phase 2: two things the live `sshd` fixture caught that a reading of the
RFCs alone did not

Both found by `apps/ssh/test_transport.sh` failing against a real `sshd`
after `apps/ssh/test.sh`'s pure fixtures already passed clean — exactly the
case `docs/ssh-decision.md` §4's two-oracle discipline exists for.

- **RFC 4253 §6's padding-alignment rule covers the 4-byte `packet_length`
  field itself** ("the length of the concatenation of 'packet_length',
  'padding_length', 'payload', and 'random padding' MUST be a multiple of
  the cipher block size or 8"), which is easy to misread as applying only to
  the *value* `packet_length` names. Before any cipher exists this matters;
  `lib/ssh.m31`'s `compute_padding_len` includes it. `chacha20-poly1305@
  openssh.com`'s own ciphered framing does NOT include it — that cipher
  encrypts its length field with a wholly separate keystream (`K_1`), so the
  4 bytes are never part of the block-aligned run RFC 4253 §6 was written
  for a generic block cipher. Two different rules, two different functions
  (`compute_padding_len` vs. `compute_padding_len_ciphered`) — conflating
  them produces "padding error ... block 8 mod 4" from a real `sshd` at
  whichever of the two framings got the other one's rule.
- **RFC 8731 §3.1's text describes the X25519 shared secret as reversed to
  big-endian before `mpint`-encoding it.** A real `sshd` rejects the host
  key's signature when that reversal is applied, and accepts it when
  `x25519`'s raw little-endian output is `mpint`-encoded directly, with no
  reversal. `lib/ssh.m31` does what OpenSSH actually ships, documented
  in-line where it matters, not what the RFC's own wording suggests —
  precisely because this library's whole testing discipline is built on not
  trusting a reading of the spec over a real implementation's behavior.

## Host keys and trust on first use (added for gitui's ssh remotes)

The library still never trusts a host by itself: `connect_any` refuses every
key it was not given, and an empty list refuses everything. What a git
client needs on top ("The authenticity of host ... can't be established.
Fingerprint SHA256:... Continue?" and, on yes, a line in `known_hosts`) is
the *presented* key, which no error could carry: `ssh.Error` is payload-free
(`docs/errors-decision.md` §4) and a freeze-level commitment, and a failure
that needs structured detail returns a type of its own.

The smallest addition that does it, all additive:

- `ssh.presented_host_key(host, port) -> Result<ssh.HostKey, ssh.Error>`
  runs the key exchange up to and including the check of the server's
  signature (so the key belongs to whoever answered), then hangs up.
  Nothing is trusted, nothing is offered, no session results.
- `ssh.HostKey` carries `algorithm`, the wire `blob` and the raw 32-octet
  `key`; `fingerprint()` is the `SHA256:<unpadded base64>` string
  `ssh-keygen -lf` prints, and `blob_base64()` the key column of a
  `known_hosts` line.
- `sshhosts.add(path, host, port, key, is_hashed)` appends the line (creating
  the file and directory; a missing final newline is supplied). Plain by
  default; `is_hashed: true` writes OpenSSH's `HashKnownHosts` form with a
  random 20-octet salt. Whether to hash is the caller's choice -- the
  library never reads `~/.ssh/config`. `plain_line` and `hashed_line` are the
  pure formatters under it.
- `kh.is_revoked(host, port, key)`: a lone `@revoked` line makes `keys_for`
  say `UnknownHost`, so a caller must check this before prompting.

The flow is `sshclient.connect` -> `KnownHosts(UnknownHost)` ->
`presented_host_key` -> show `fingerprint()` -> on yes `add` -> connect
again. The second connection is the same refusing check as any other, so the
key the person approved is the only one that can succeed; the probe
connection only chooses what to *show*. A `Connect(HostKeyMismatch)` is a hard
refusal, with `presented_host_key` available to show what was offered; there
is no `add` path for it.

One change to existing behaviour: the handshake now verifies the server's
signature *before* comparing the key with the expected ones (it used to
refuse a wrong key first), so `HostKeyMismatch` always names a key the
server really holds. A wrong-key server that also signs badly now gets
`BadSignature`.

Tested in `apps/ssh/t_ssh_hosts.m31` (formatters, round trips, `is_revoked`)
and live in `apps/ssh/test_auth_exec.sh`: the fingerprint equals
`ssh-keygen -lf` of the real host key; files written by `add` (plain,
hashed, new directory, no trailing newline) are found by `ssh-keygen -F` and
accepted by the real `ssh` with `StrictHostKeyChecking=yes`; a second sshd
with a different host key makes a changed key a `HostKeyMismatch`, which the
real `ssh` also refuses.
