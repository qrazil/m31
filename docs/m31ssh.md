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
       [-J jump[,jump...]] [-L spec] [-R spec] [-N]
       [-t|-tt|-T] [-n] [-e char|none] [-q] [user@]host [command ...]
```

| | |
|---|---|
| no command, stdin is a terminal | a login shell on a pty; the local terminal goes raw |
| no command, stdin is not a terminal | a shell with no pty reading commands from stdin |
| a command | `exec`; a pty only with `-t`/`-tt` (or `RequestTTY`) |
| `-J a,b` / `ProxyJump` | the connection to the destination is made through `a` and `b` (see below) |
| `-L [bind:]port:host:hostport` | listen locally; connections go out through the server to `host:hostport` |
| `-R [bind:]port:host:hostport` | the server listens; connections come back to `host:hostport` from here (`port` 0: the server chooses and the client prints it) |
| `-N` | no remote command: the connection only carries forwards, until killed or dropped |
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
`ProxyJump`, `LocalForward` and `RemoteForward`. `Include` is expanded in
place (relative to `~/.ssh`, `~` and globs, lexical order, nested up to 16
deep, a missing file is skipped). `Match` understands `all`, `host`,
`originalhost`, `user` and `localuser` (comma-separated patterns, `!`
negates); a `Match` with any other criterion (`exec`, `canonical`, ...) is
reported and its lines are *not* applied (so a security setting inside one is
not half applied). Any other keyword is reported-and-ignored. A malformed value is an error, never a default.

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
`~/.ssh/id_ed25519`, `id_ecdsa`, `id_rsa`). A key present in both is signed
through the agent. `IdentitiesOnly yes` restricts the agent to keys that also
have a listed key file. A key file readable by group or others is refused
with a warning (like OpenSSH's "UNPROTECTED PRIVATE KEY FILE"). Each rejected
key moves on to the next; none left is `Permission denied (publickey).`,
exit 255.

Key types: **ssh-ed25519**, **ecdsa-sha2-nistp256** and **ssh-rsa** (2048 to
4096 bits; signed with `rsa-sha2-512`, then `rsa-sha2-256`: never SHA-1). A
passphrase-protected key (`openssh-key-v1` with `aes256-ctr`, `aes128-ctr`,
`aes256-gcm@openssh.com` or `chacha20-poly1305@openssh.com`, bcrypt KDF) is
decrypted only when its turn comes and no agent key already covers it: the
prompt is `Enter passphrase for key 'file': ` on the terminal with echo off,
three tries, Ctrl-C skips the key, and with no terminal the key is skipped
with a message (use the agent). The passphrase is never printed, logged,
placed in an error, or taken from the environment or the command line. It
is not wiped from memory afterwards (the runtime cannot), and a cost over
1024 bcrypt rounds in a file is refused rather than run.

### Algorithms

Offered, strongest first (and **nothing else is ever accepted**: no SHA-1
anything, no CBC, no `none`, no compression, no `diffie-hellman-*`):

| | |
|---|---|
| key exchange | `curve25519-sha256` (and its `@libssh.org` name), `ecdh-sha2-nistp256`; `kex-strict-c-v00@openssh.com` marker |
| host key | `ssh-ed25519`, `ecdsa-sha2-nistp256`, `rsa-sha2-512`, `rsa-sha2-256` (2048-4096 bit keys) |
| cipher | `chacha20-poly1305@openssh.com`, `aes256-gcm@openssh.com`, `aes128-gcm@openssh.com`, `aes256-ctr`, `aes128-ctr` |
| MAC (CTR only) | `hmac-sha2-256-etm@openssh.com`, `hmac-sha2-256` |

The client's order decides (RFC 4253 §7.1), with one adaptation: host-key
algorithms the host already has in `known_hosts` go first, so a server with
several keys presents the one the file can vouch for. If the server shares
none of a category the error says which category and the connection ends.
**Strict kex** (OpenSSH 9.6, the Terrapin fix) is used whenever the server
advertises it: nothing but KEXINIT, the exchange and NEWKEYS is accepted
during the key exchange, and sequence numbers restart at 0 after NEWKEYS.

### ProxyJump

`-J [user@]host[:port][,...]` or `ProxyJump` opens a `direct-tcpip` channel
on an authenticated connection to the jump host and runs the next hop's
whole SSH connection over it. Every hop is checked against **its own**
`known_hosts` entry and authenticates for itself; the jump host sees only
the inner session's ciphertext. As `ssh(1)` says for `-J`, command-line
options (`-o`, `-i`, `-p`, `-l`) apply to the destination only; a jump host
is configured through `~/.ssh/config` (`Host` blocks with its own `User`,
`Port`, `IdentityFile`, `UserKnownHostsFile`, ...).

### Forwarding

`-L`, `-R` and `LocalForward`/`RemoteForward`, any number, any mix with a
session. One thread polls the SSH socket, the keyboard, every listener and
every forwarded socket (docs below, "How it works"). Flow control is end to
end: a slow local program stops the server (window credit is returned only
as data is consumed; at most 2 MiB is held per tunnel) and a slow server
stops the local program (a socket is read only as far as the server's
window allows). EOF is carried both ways (half-close works), and a server
CLOSE retires a tunnel only after what it sent has reached the local
socket. More than 64 simultaneous tunnels are refused (descriptor and
memory bound). A forward that cannot be set up is a warning, not a failure
(OpenSSH's `ExitOnForwardFailure no`); `-N` with none working is an error.
The listener binds loopback unless a bind address is given (`*` for all).
When the session channel ends the client exits and its tunnels with it
(OpenSSH waits for them).

## The other programs

`m31scp`, `m31sftp` and `m31ssh-keygen` are built like `m31ssh`
(`bash apps/build.sh NAME`), share its connection logic
(`lib/ssh/sshconnect.m31`: configuration, host keys, identities, jump hosts,
so they trust the same hosts the same way) and are **unaudited** too.

```
m31scp [-P port] [-i identity] [-o opt=val] [-F config] [-J jump] [-r] [-p] [-q]
       source ... target                    # one side [user@]host:path
m31sftp [-P port] [-i identity] [-o opt=val] [-F config] [-J jump] [-b file] [-q]
        [user@]host[:directory]
m31ssh-keygen [-t ed25519] [-f file] [-N passphrase] [-C comment] [-a rounds] [-q]
m31ssh-keygen -y [-f file] [-P passphrase]      # public key from a private key
m31ssh-keygen -l [-f file]                      # SHA-256 fingerprint
m31ssh-keygen -R host [-f known_hosts]          # remove a host (keeps .old)
```

* **SFTP version 3** (`lib/ssh/sshsftp.m31`): `stat lstat list mkdir rmdir
  remove rename realpath chmod`, whole-file `get` and `put` pipelined 16
  requests deep with 32 KiB per request, recursive trees, and a progress
  meter on a terminal. A server's directory entry names are treated as
  hostile: an empty name, `.`, `..` or a name with `/` or NUL is refused, so
  a server cannot write outside the directory the user named. A short read is
  re-requested; any mismatched id, over-long reply or implausible count is an
  error, never a trap.
* **m31scp** speaks SFTP (as OpenSSH's own `scp` has since 9.0), so it does
  not need a shell or a remote `scp`; remote-to-remote copies are not
  supported, `-r` is needed for a directory, several sources need a
  directory target. Exit 0, 1 (transfer or usage), 255 (connection).
  Downloaded files get the remote's permission bits (before the umask);
  `-p` sets the remote modification time after an upload (the language
  has no way to set one locally, so downloads keep their own time).
* **m31sftp** reads `ls [-l] cd lcd pwd lpwd get [-r] put [-r] mkdir rmdir
  rm rename stat chmod help quit` from the terminal (with a prompt), a pipe,
  or `-b file`; in batch mode the first failure ends it with 1.
* **m31ssh-keygen** generates Ed25519 keys (the file is `openssh-key-v1`,
  optionally `aes256-ctr` + bcrypt with a fresh salt, `-a` rounds, default 16),
  mode 0600 beside a 0644 `.pub`; it never overwrites. `-y` and `-l` work on
  ed25519, ECDSA and RSA keys and on public-key and known_hosts files; `-R`
  removes plain, `[host]:port` and hashed entries and keeps the old file as
  `.old`. A passphrase given with `-N`/`-P` is visible in the process list:
  prefer the prompt. RSA and ECDSA keys cannot be generated (no key
  generation for them: it needs prime generation, which is not here).

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
* **Algorithm reach.** Servers that offer none of the suites above cannot be
  reached: old servers that only have `diffie-hellman-group*`, `ssh-rsa`
  host keys signed with SHA-1, CBC ciphers or 1024-bit RSA host keys are
  refused on purpose, and so are ECDSA P-384/P-521 and `ssh-dss`. There is no
  certificate support (`*-cert-v01@openssh.com`, `@cert-authority` lines are
  skipped) and no FIDO (`sk-*`) keys.
* **Key files**: `aes*-cbc`, `3des-cbc` and `aes192-ctr` key encryption is
  refused by name (the library has no AES decryption direction or 192-bit
  key schedule); re-encrypt such a key with `ssh-keygen -p -Z aes256-ctr`.
* **The AES implementation is slow** (about 1 MiB/s: it is the table-free,
  constant-time-in-the-source one). ChaCha20-Poly1305 is first in the offer
  for that reason; against an AES-only server expect megabytes per second,
  not tens.
* **bcrypt/Blowfish is not constant time** (S-box lookups by
  password-derived data, as in every bcrypt); RSA signing, ECDSA signing,
  AES, ChaCha20 and X25519 are written to be, at the source level only.
* **No ssh-agent forwarding, X11, `-D` (SOCKS), `-W`, UNIX-socket
  forwarding, multiplexing, `Compression`, GSSAPI,
  password or keyboard-interactive authentication.**
* **Resize latency** is up to ~100 ms (a poll, not a signal).
* **Terminal escape injection** from the *remote program's* output is the
  terminal's business, as with any ssh; only the server banner and error
  strings are sanitised here.

## How it works (the part that is not in the RFCs)

The language has no signal handlers and no multi-descriptor wait except
`poll`, so the interactive loop (`lib/ssh/sshshell.m31`) is one thread around
`poll(2)` on the SSH socket, the keyboard and every forwarded socket
(`lib/ssh/sshforward.m31`), with a 100 ms tick. Transport layers are
`ssh.Wire`s, so a `ProxyJump` hop is a channel on the previous connection
(`lib/ssh/sshjump.m31`), whose `wait` is exact: it never reports readable
for a jump-host message that carries no inner bytes.

* the SSH socket is an `ssh.PollWire`: every wait for it is a `poll(2)` on
  the program's own thread (`net.connect_polled` for the connect), never a
  park on the reactor, because this program has one thread of control and
  nothing else to schedule;
* it reads **one packet per readiness** (`Transport.recv_once`): OpenSSH
  sends `SSH_MSG_IGNORE` chaff, and a `recv` that swallowed one and then
  waited for the next real packet would stall the keyboard on an idle
  connection (a bug found by a stress test, now a regression test: see the
  `-N` check in `test_m31ssh.sh`);
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

`bash tests/ssh/test_m31ssh.sh` builds the program and runs 136 checks against
disposable OpenSSH `sshd`s (temp host keys of three types, temp user keys of
three types, a temp `HOME`, ephemeral `127.0.0.1` ports, private
`ssh-agent`s); nothing touches the real `~/.ssh` and every process is killed
on exit, including on an exception or a signal. It also runs a cipher /
key-exchange / host-key matrix against restricted servers, three downgrade
refusals, encrypted keys through a pty prompt, ProxyJump chains (with a
resize through a jump), and `-L`/`-R`/`-N` with multi-megabyte, concurrent
and half-closed connections. `bash tests/ssh/test.sh` adds the oracles:
`oracle_sshcipher.py` (677 packet vectors against OpenSSL, every single-byte
corruption refused), `oracle_sshkeyfiles.py` (every key type, plain and in
four encrypted formats, against `ssh-keygen -y` and OpenSSL),
`oracle_rsasign.py` (signatures byte-identical to OpenSSL's) and the
bcrypt_pbkdf published vector. `bash tests/ssh/test_m31sftp.sh` runs 56
checks for `m31scp`, `m31sftp` and `m31ssh-keygen` against a disposable
`sshd` with its internal SFTP server: 20 MB and empty files, trees with
modes, the error paths, a progress meter on a pty, interoperation in both
directions with OpenSSH's own `scp`, keys written by `m31ssh-keygen` read
by `ssh-keygen -y`/`-l` and used to log in with both clients, and `-R`
checked with `ssh-keygen -F`. A Python pty driver runs
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

## Status by phase

* **A** (shell, exec, pty, resize, `~.`, agent, `known_hosts`, config,
  keepalives): done.
* **B** (RSA and ECDSA keys, encrypted keys, ProxyJump, `-L`/`-R`/`-N`,
  more ciphers / key exchanges): done, with the limits above.
* **C** (`m31scp`, an sftp v3 subset, `m31ssh-keygen`): done, see "The other
  programs".
