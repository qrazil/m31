# Standard library layout: groups (0.4.0)

Status: decided and implemented in 0.4.0.

`lib/` used to be 58 files in one directory. It is now grouped:

| Group | Modules |
|---|---|
| `crypto` | aes aesgcm bignum chacha20poly1305 consttime ecdsa ecdsasign ed25519 field25519 hkdf hmac rsa scalar25519 sha1digest sha256 sha512 x25519 |
| `encoding` | base64 csv der hexcodec html json |
| `net` | deadlinestream http https |
| `pki` | clientcert ocsp signingkey x509 x509chain |
| `ssh` | sshauth sshclient sshexec sshhosts sshkey |
| `text` | diff regex unicode |
| `tls` | tls12 tls13record tls13schedule tlsresume tlsserver |

Modules that are the everyday surface stay at the top of `lib/`: `args date
fs io math net os random sort term text timer tls ssh`. (`net.m31`,
`text.m31`, `tls.m31` and `ssh.m31` sit beside directories of the same name
-- the module `net` and the group `net` do not conflict, see below.)

## The rule

`import crypto.sha256;` reads `lib/crypto/sha256.m31`, and the module is then
called by its **name alone**: `sha256.hex(..)`, never `crypto.sha256.hex(..)`.
This is the same rule as for a project's own directories
(docs/project-layout-decision.md §3): the last segment is the module's whole
identity and module names stay globally unique. Namespacing was decided
against and this does not reopen it; a directory is for finding a file.

Consequences, all enforced by the compiler:

- A grouped module is importable only by its full path. `import sha256;` is
  an error that says to write `import crypto.sha256;`.
- `import crypto.nosuch;` names the group in the error.
- A project file whose name equals a stdlib module (`crypto/sha256.m31` in
  your project, or a `sha256.m31` anywhere) collides rather than overrides,
  as for the flat modules.
- A dependency in `deps` named like a group (`crypto`) makes `crypto.sha256`
  ambiguous and is refused; rename the dependency.
- The embedded table in `src/stdlib.rs` is keyed by the dotted path; a unit
  test checks it is exactly the set of files under `lib/`, and that no two
  modules share a name across groups.

## Why these groups

They follow the real import graph, which is layered and acyclic: `crypto`
depends only on `encoding.hexcodec`; `pki` on `encoding` and `net`; `tls` and
`ssh` on `crypto`, `pki`, `net` and `io`; `net.https` on `net.http` and
`tls`. A group never imports from a group above it, so the directories read
as the dependency order.

## Cost

Every flat `import sha256;` of a grouped module changed. Inside this
repository that was rewritten mechanically (the emitted C of every corpus,
example and app program is byte-identical to before; only import lines
changed). External repositories pin an m31 version (`M31_REF`), so they move
when they choose to; there is no flat-name shim, because a shim would be a
second spelling of every module.
