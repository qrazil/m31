//! The standard library, carried inside the compiler binary.
//!
//! Every module here is an ordinary source file in `lib/`, compiled by this
//! compiler with the same diagnostics and the same gates as a program. The
//! only thing it may do that a program may not is write `prim` -- see
//! docs/stdlib-seam.md.
//!
//! It is embedded rather than installed because every other answer has a way
//! to be wrong on the user's machine: Go has `GOROOT`, C has an include path
//! that differs per distribution, Python has `sys.path`. The compiler is one
//! binary with no dependencies, and the standard library does not change
//! that.

/// Every importable module: its path as an `import` spells it -- dots, the
/// directories of `lib/` -- and its source.
///
/// A path of one segment (`io`) is a module that lives directly in `lib/`;
/// a longer one (`crypto.sha256`) is a module in a group, the directory of
/// the same name. The last segment is the module's NAME, which is its whole
/// identity (docs/project-layout-decision.md §3): `sha256.digest(..)`, never
/// `crypto.sha256.digest(..)`. Groups are for finding things, not for
/// namespacing, so two modules still cannot share a name, in different
/// groups or otherwise -- see docs/stdlib-layout-decision.md.
const MODULES: &[(&str, &str)] = &[
    (
        "encoding.base64",
        include_str!("../lib/encoding/base64.m31"),
    ),
    ("encoding.csv", include_str!("../lib/encoding/csv.m31")),
    ("encoding.der", include_str!("../lib/encoding/der.m31")),
    ("text.diff", include_str!("../lib/text/diff.m31")),
    ("encoding.html", include_str!("../lib/encoding/html.m31")),
    ("net.http", include_str!("../lib/net/http.m31")),
    ("net.https", include_str!("../lib/net/https.m31")),
    ("io", include_str!("../lib/io.m31")),
    ("fs", include_str!("../lib/fs.m31")),
    ("encoding.json", include_str!("../lib/encoding/json.m31")),
    ("math", include_str!("../lib/math.m31")),
    ("net", include_str!("../lib/net.m31")),
    ("os", include_str!("../lib/os.m31")),
    ("sort", include_str!("../lib/sort.m31")),
    ("term", include_str!("../lib/term.m31")),
    ("text", include_str!("../lib/text.m31")),
    ("date", include_str!("../lib/date.m31")),
    ("timer", include_str!("../lib/timer.m31")),
    ("random", include_str!("../lib/random.m31")),
    ("text.regex", include_str!("../lib/text/regex.m31")),
    ("args", include_str!("../lib/args.m31")),
    ("text.unicode", include_str!("../lib/text/unicode.m31")),
    (
        "crypto.field25519",
        include_str!("../lib/crypto/field25519.m31"),
    ),
    ("crypto.sha512", include_str!("../lib/crypto/sha512.m31")),
    ("crypto.x25519", include_str!("../lib/crypto/x25519.m31")),
    (
        "crypto.scalar25519",
        include_str!("../lib/crypto/scalar25519.m31"),
    ),
    ("crypto.ed25519", include_str!("../lib/crypto/ed25519.m31")),
    ("pki.ocsp", include_str!("../lib/pki/ocsp.m31")),
    (
        "crypto.sha1digest",
        include_str!("../lib/crypto/sha1digest.m31"),
    ),
    ("crypto.sha256", include_str!("../lib/crypto/sha256.m31")),
    (
        "crypto.chacha20poly1305",
        include_str!("../lib/crypto/chacha20poly1305.m31"),
    ),
    ("crypto.aes", include_str!("../lib/crypto/aes.m31")),
    ("crypto.aesgcm", include_str!("../lib/crypto/aesgcm.m31")),
    ("ssh", include_str!("../lib/ssh.m31")),
    ("ssh.sshkey", include_str!("../lib/ssh/sshkey.m31")),
    ("ssh.sshhosts", include_str!("../lib/ssh/sshhosts.m31")),
    ("ssh.sshauth", include_str!("../lib/ssh/sshauth.m31")),
    ("ssh.sshexec", include_str!("../lib/ssh/sshexec.m31")),
    ("ssh.sshclient", include_str!("../lib/ssh/sshclient.m31")),
    (
        "crypto.consttime",
        include_str!("../lib/crypto/consttime.m31"),
    ),
    (
        "encoding.hexcodec",
        include_str!("../lib/encoding/hexcodec.m31"),
    ),
    ("crypto.hmac", include_str!("../lib/crypto/hmac.m31")),
    ("crypto.hkdf", include_str!("../lib/crypto/hkdf.m31")),
    ("crypto.bignum", include_str!("../lib/crypto/bignum.m31")),
    ("crypto.ecdsa", include_str!("../lib/crypto/ecdsa.m31")),
    (
        "crypto.ecdsasign",
        include_str!("../lib/crypto/ecdsasign.m31"),
    ),
    ("pki.signingkey", include_str!("../lib/pki/signingkey.m31")),
    ("pki.x509", include_str!("../lib/pki/x509.m31")),
    ("pki.x509chain", include_str!("../lib/pki/x509chain.m31")),
    ("crypto.rsa", include_str!("../lib/crypto/rsa.m31")),
    (
        "tls.tls13schedule",
        include_str!("../lib/tls/tls13schedule.m31"),
    ),
    (
        "tls.tls13record",
        include_str!("../lib/tls/tls13record.m31"),
    ),
    ("tls.tls12", include_str!("../lib/tls/tls12.m31")),
    ("pki.clientcert", include_str!("../lib/pki/clientcert.m31")),
    ("tls.tlsresume", include_str!("../lib/tls/tlsresume.m31")),
    ("tls", include_str!("../lib/tls.m31")),
    ("tls.tlsserver", include_str!("../lib/tls/tlsserver.m31")),
    (
        "net.deadlinestream",
        include_str!("../lib/net/deadlinestream.m31"),
    ),
];

/// The last segment of an import path: the module's name.
fn name_of(path: &str) -> &str {
    path.rsplit('.').next().unwrap_or(path)
}

/// The source of the embedded module an `import` of exactly this path names
/// (`crypto.sha256`, or `io` for a module outside every group), or `None`.
pub fn by_path(path: &str) -> Option<&'static str> {
    MODULES.iter().find(|(p, _)| *p == path).map(|(_, s)| *s)
}

/// The path an import must spell for the embedded module of this name
/// (`sha256` -> `crypto.sha256`), or `None` if the name is not one.
pub fn path_of(name: &str) -> Option<&'static str> {
    MODULES
        .iter()
        .find(|(p, _)| name_of(p) == name)
        .map(|(p, _)| *p)
}

/// Is this the name of a group -- a directory of `lib/` -- rather than of a
/// module? `import crypto;` names nothing, while `import crypto.sha256;`
/// reaches into one.
pub fn is_group(name: &str) -> bool {
    MODULES
        .iter()
        .any(|(p, _)| p.split_once('.').is_some_and(|(group, _)| group == name))
}

/// The source of an embedded module a program may import, by NAME -- the
/// last segment of its path -- or `None` if the name is not one.
///
/// A module named here is taken: a file of the same name beside the program,
/// or in any directory under it, is a collision, not an override, because
/// module names are globally unique (docs/modules-decision.md §1).
pub fn source(name: &str) -> Option<&'static str> {
    path_of(name).and_then(by_path)
}

/// The module that turns floats into text and back. The compiler lowers
/// `print` of a float, `to_str`, `str()` and `parse_float` to calls into it,
/// and the loader adds it to any program that could reach one of those.
///
/// No program can import it or collide with it: the name begins with `__`,
/// which the parser refuses in every name a program writes, `import`
/// included. See docs/stdlib-seam.md §6.
pub const FLOATFMT: &str = "__floatfmt";

/// The module that decodes a `str` into code points and encodes them back.
/// The compiler lowers `s.chars()` and `str.from_chars(xs)` to calls into
/// it, and the loader adds it to any program that mentions either -- the
/// same arrangement as `FLOATFMT`, for the same reason: UTF-8 is bit
/// manipulation the language can write, so the runtime does not.
pub const TEXT: &str = "__text";

/// The source of any embedded module, importable or not -- for the loader,
/// which also brings in `FLOATFMT` and `TEXT`, and for quoting a line of it
/// in a diagnostic.
pub fn embedded(name: &str) -> Option<&'static str> {
    if name == FLOATFMT {
        return Some(include_str!("../lib/__floatfmt.m31"));
    }
    if name == TEXT {
        return Some(include_str!("../lib/__text.m31"));
    }
    source(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `.m31` file under `lib/` is in the table at the path its
    /// directory spells, and the table has nothing else: a file moved without
    /// its entry (or the reverse) is a build that quietly lacks a module.
    #[test]
    fn the_table_is_exactly_the_files_under_lib() {
        fn walk(dir: &std::path::Path, prefix: &str, out: &mut Vec<String>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                let n = p.file_name().unwrap().to_string_lossy().to_string();
                if p.is_dir() {
                    walk(&p, &format!("{}{}.", prefix, n), out);
                } else if let Some(stem) = n.strip_suffix(".m31") {
                    if !stem.starts_with("__") {
                        out.push(format!("{}{}", prefix, stem));
                    }
                }
            }
        }
        let mut found = Vec::new();
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("lib"),
            "",
            &mut found,
        );
        found.sort();
        let mut table: Vec<String> = MODULES.iter().map(|(p, _)| p.to_string()).collect();
        table.sort();
        assert_eq!(found, table);
    }

    #[test]
    fn module_names_are_unique_across_groups() {
        let mut names: Vec<&str> = MODULES.iter().map(|(p, _)| name_of(p)).collect();
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len());
    }
}
