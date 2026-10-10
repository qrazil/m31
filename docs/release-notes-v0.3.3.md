# m31 v0.3.3

Not tagged yet. Suggested tag message:

    m31 0.3.3: ssh client (publickey auth, known_hosts, exec channel) with caller-driven trust on first use, terminal reads park only the green thread

## What is new since v0.3.2

(`git log v0.3.2..master` is two commits; the TLS, net-gaps, poll-timeout and
kqueue work that shipped around v0.3.2 is already in v0.3.2 and is not new.)

### An ssh client in the standard library

Everything a program needs for `ssh user@host command`, validated against a
disposable OpenSSH 9.6 `sshd` with throwaway keys (`apps/ssh/test_auth_exec.sh`):

- `lib/sshclient.m31`: `sshclient.connect(host, port, user, key_path,
  known_hosts_path)` -- connect, check the host key, authenticate, run a
  command, in one call.
- `lib/sshkey.m31`: unencrypted `openssh-key-v1` Ed25519 private keys
  (passphrase-protected ones are refused with a clear error).
- `lib/sshhosts.m31`: OpenSSH `known_hosts` -- plain, `[host]:port`, wildcards,
  negation, `@revoked`, and hashed (`ssh-keygen -H`) entries.
- `lib/sshauth.m31`: RFC 4252 `publickey` authentication.
- `lib/sshexec.m31`: an RFC 4254 session channel with `exec`, flow control,
  stdin, separate stdout/stderr and the exit status.
- `ssh.connect_any` (a list of acceptable host keys) and the `ssh.Link`
  interface the upper layers run over.

Host keys, for programs that ask a person ("the authenticity of host ... can't
be established"): `ssh.presented_host_key(host, port)` returns the key the
server offers (its signature verified, nothing trusted); `HostKey.fingerprint()`
is the `SHA256:...` string `ssh-keygen -lf` prints; `sshhosts.add(path, host,
port, key, hashed)` records it, plain or in `HashKnownHosts` form;
`KnownHosts.is_revoked` guards the prompt. The library still never trusts a
host on its own, and a changed key is still a hard `HostKeyMismatch`.

Known limits: Ed25519 keys and host keys only, no rekeying, no password or
agent auth, no passphrase-protected keys. See `docs/ssh-decision.md`.

### Terminal reads park only the green thread

`term.Reader.read` waits through the reactor (`__wait_io_timeout`) instead of
blocking the carrier in `poll(2)`, so a program with other green threads keeps
them running while it waits for a key. A descriptor the reactor cannot wait on
(a regular file, `/dev/null`) reads as ready, as before. New gate: "terminal
reads park only the green thread".

### Behaviour change to know about

`ssh.connect`/`connect_any` now verify the server's signature before comparing
its host key with the expected ones; a wrong-key server that also signs badly
reports `BadSignature` instead of `HostKeyMismatch`.
