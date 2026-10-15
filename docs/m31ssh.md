# m31ssh

> **UNAUDITED.** `m31ssh` is an SSH client written from scratch in m31, on a
> from-scratch crypto stack, and tested against OpenSSH. It has not been
> reviewed by anyone who audits cryptographic software, and it has not been
> hardened against a determined network attacker. Prefer OpenSSH wherever
> that matters. The `--help` text says so too.

`apps/m31ssh` is the program; the protocol is `lib/ssh.m31` and
`lib/ssh/*.m31`, the primitives `lib/crypto/*.m31`. The decision record for
the library is `docs/ssh-decision.md`; this file is about the program.

## Use

```
m31ssh [-p port] [-i identity] [-l user] [-o option=value] [-F config]
       [-t|-tt|-T] [-n] [-e char|none] [-q] [user@]host [command ...]
```

| | |
|---|---|
| no command, stdin is a terminal | a login shell on a pty; the local terminal goes raw |
| no command, stdin is not a terminal | a shell with no pty reading commands from stdin |
| a command | `exec`; a pty only with `-t`/`-tt` (or `RequestTTY`) |
| exit status | the remote command's; `128+n` for death by signal; **255** for an ssh error |
| `~.` | at the start of a line: disconnect (exit 255). `~~` sends one `~`; `~?` lists them. `-e none` turns the escape off |
| Ctrl-C, Ctrl-D, Ctrl-Z | bytes, as in OpenSSH: they reach the *remote* terminal |
| resize | the new size is sent within ~100 ms (see "known limits") |

Options come from, in this order, the command line, then `~/.ssh/config` (or
`-F`); **the first value obtained wins**. Understood keywords: `Host`
(patterns with `*`, `?`, `!`), `HostName`, `User`, `Port`, `IdentityFile`
(accumulates; `~`, `%h %u %r %d %%` expand), `IdentitiesOnly`,
`StrictHostKeyChecking` (`yes`, `ask`, `accept-new`, `no`), `UserKnownHostsFile`,
`ServerAliveInterval`, `ServerAliveCountMax`, `ConnectTimeout`, `RequestTTY`,
and `ProxyJump`/`LocalForward`/`RemoteForward` which are parsed and then
refused with a clear message until they are implemented. `Match`, `Include`
and any other keyword are reported-and-ignored, and the lines of a `Match`
block are *not* applied (so a security setting inside one is not half
applied). A malformed value is an error, never a default.

### Host keys

`known_hosts` (plain, `[host]:port`, hashed; `@revoked`) is consulted **before
any credential is offered**. The decision is taken mid-handshake, after the
server has proved it holds the key, from one connection.

| situation | result |
|---|---|
| key matches | continue |
| host unknown, `ask` (default), stdin is a terminal | prompt with the fingerprint; only `yes` continues; the key is then appended |
| host unknown, `ask`, no terminal | **refused** (255), nothing written |
| host unknown, `yes` | refused |
| host unknown, `accept-new` | recorded and continue, with a one-line notice |
| host unknown, `no` | recorded and continue (no prompt) |
| **key differs from the recorded one** | **hard refusal in every mode, including `no`**: the message names the key the server offered and says nothing was modified; the file is never rewritten, and there is no "continue anyway" |
| key is `@revoked` / file is malformed / unreadable | refused |

(OpenSSH's `StrictHostKeyChecking=no` goes on with a changed key; this client
deliberately does not.)

### Identities

Candidates are offered in this order: keys held by the ssh-agent
(`SSH_AUTH_SOCK`), then key files (`-i`, `IdentityFile`, default
`~/.ssh/id_ed25519`). A key present in both is signed through the agent.
`IdentitiesOnly yes` restricts the agent to keys that also have a listed key
file. A key file readable by group or others is refused with a warning (like
OpenSSH's "UNPROTECTED PRIVATE KEY FILE"). Each rejected key moves on to the
next; none left is `Permission denied (publickey).`, exit 255.

## Threat model

What this client tries to give you, assuming a network attacker who can read,
modify, drop and inject packets and who runs a hostile server:

1. **Server authenticity** against the pinned host key: the exchange hash is
   signed by the host key (Ed25519, verified in constant time over public
   data), and the key is compared with `known_hosts` before authentication.
   A man in the middle without the host key cannot complete the handshake.
2. **Confidentiality and integrity** of the stream:
   `curve25519-sha256` + `chacha20-poly1305@openssh.com`. The Poly1305 tag is
   verified (constant-time compare) **before** the payload is decrypted or
   interpreted, and before the length is trusted beyond a bound.
3. **No downgrade**: exactly one key exchange, one host key algorithm and one
   cipher are offered; a server that does not offer them gets a clear error,
   never a weaker fallback. There is no compression, no `none` cipher.
4. **Malicious server input does not crash or hang the client**: every length
   field is bounds-checked, packets are capped (35000 bytes), banners and
   names are sanitised before printing, the agent's replies are capped and
   its signatures verified, a channel never accepts more data than the window
   we granted. `tests/ssh/test_m31ssh.sh` runs hostile servers (garbage, an
   endless version line, absurd lengths, truncated KEXINIT, immediate
   closes), a silent server, and a server that stops answering.
5. **Secrets stay out of output**: no error or log carries key material; the
   private key's seed is private to `sshkey`; `sshkey.Error` is payload-free.
6. **The local terminal is restored** on every exit path the language can
   see (return, `?`, trap, `os.exit`); and `known_hosts` is only ever appended
   to, after an explicit yes (or `accept-new`/`no`).

What it does **not** defend against, and where the limits are:

* **Side channels beyond constant-time code.** The field arithmetic and the
  tag comparison are written to be constant time, but m31 compiles to C and
  the C compiler, the CPU and the OS are not audited for it. The Ed25519
  *verification* is over public data. Signing uses the secret seed.
* **Memory hygiene.** m31 gives no way to lock or reliably zeroise memory;
  key material can linger in freed memory, swap or a core dump.
* **The agent is trusted** to sign what it is shown, as with OpenSSH.
* **Local attackers** who can ptrace the process or read its memory.
* **Denial of service**: a server can hold a connection open; keepalives and
  `ConnectTimeout` are the only bound (no connect timeout for the TCP
  connect itself beyond the OS's).
* **No rekeying.** A session that transfers more than the cipher's safe
  volume (OpenSSH rekeys at 1 GB / 1 hour) would need it; this client does
  not rekey and a server that demands it ends the session with an error.
* **A single algorithm suite**: servers that do not offer
  `curve25519-sha256` + `ssh-ed25519` + `chacha20-poly1305@openssh.com`
  cannot be reached yet (see the roadmap below).
* **Terminal escape injection** from the *remote program's* output is the
  terminal's business, as with any ssh; only the server banner and error
  strings are sanitised here.

## How it works (the part that is not in the RFCs)

The language has no signal handlers and no multi-descriptor wait except
`poll`, so the interactive loop (`lib/ssh/sshshell.m31`) is one thread around
`poll(2)` on the socket and the keyboard with a 100 ms tick:

* it drains at most 32 packets per turn so the keyboard is never starved
  (Ctrl-C works against `yes`);
* received data is written to the terminal after every packet, so memory is
  bounded by a packet plus the 2 MiB window;
* keyboard input is sent only as far as the server's window allows
  (`Channel.write_available`); the rest is held, and the keyboard is not
  polled until it drains, which is the backpressure that keeps
  `ssh host cat < huge` correct without ever blocking the output;
* resize is noticed by polling the terminal size on each tick (no SIGWINCH
  handler exists, `docs/sys-layer.md` §2): latency up to ~100 ms;
* keepalives (`ServerAliveInterval`) are `keepalive@openssh.com` global
  requests; any packet from the server resets the count; more than
  `ServerAliveCountMax` unanswered ends the session ("Timeout, server ...
  not responding", 255).

## Tests

`bash tests/ssh/test_m31ssh.sh` builds the program and runs 66 checks against
a disposable OpenSSH `sshd` (temp host key, temp user keys, a temp `HOME`,
an ephemeral `127.0.0.1` port, a private `ssh-agent`); nothing touches the
real `~/.ssh` and every process is killed on exit. A Python pty driver runs
interactive sessions: command output and exit status, `stty size` before and
after a resize, 200000 lines on a pty, Ctrl-D, `~.`, `~.` mid-line, Ctrl-C
passthrough, terminal restoration (termios read back), host key mismatch
(fingerprint named, file byte-identical afterwards), unknown host
accept/reject on a pty and refusal without one, `accept-new`, `yes`, agent
signing, `IdentitiesOnly`, a wrong key, an over-permissive key file, config
files and command-line precedence, keepalive against a SIGSTOPped server,
a silent server, and hostile servers. Synchronisation is on observable output
only; timeouts exist to bound failures. The pure parts have their own unit
tests (`t_ssh_config`, `t_ssh_shell`, `t_ssh_agent`) run by `tests/ssh/test.sh`,
whose existing oracles (OpenSSH `sshd`, `ssh-keygen`, `cryptography`,
`hashlib`) cover the library underneath.

## Roadmap

Phase A (this file) is done. Phase B (RSA and ECDSA keys, encrypted keys,
`-J`, `-L`/`-R`/`-N`, more algorithms) and Phase C (`m31scp`, sftp subset,
`m31ssh-keygen`) are tracked at the end of this file as they land.
