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

/// The source of an embedded module a program may import, or `None` if the
/// name is not one.
///
/// A module named here is taken: a file of the same name beside the program
/// is a collision, not an override, because module names are globally unique
/// (docs/modules-decision.md §1).
pub fn source(name: &str) -> Option<&'static str> {
    Some(match name {
        "base64" => include_str!("../lib/base64.m31"),
        "csv" => include_str!("../lib/csv.m31"),
        "der" => include_str!("../lib/der.m31"),
        "diff" => include_str!("../lib/diff.m31"),
        "html" => include_str!("../lib/html.m31"),
        "http" => include_str!("../lib/http.m31"),
        "io" => include_str!("../lib/io.m31"),
        "fs" => include_str!("../lib/fs.m31"),
        "json" => include_str!("../lib/json.m31"),
        "math" => include_str!("../lib/math.m31"),
        "net" => include_str!("../lib/net.m31"),
        "os" => include_str!("../lib/os.m31"),
        "sort" => include_str!("../lib/sort.m31"),
        "term" => include_str!("../lib/term.m31"),
        "text" => include_str!("../lib/text.m31"),
        "date" => include_str!("../lib/date.m31"),
        "random" => include_str!("../lib/random.m31"),
        "regex" => include_str!("../lib/regex.m31"),
        "args" => include_str!("../lib/args.m31"),
        "unicode" => include_str!("../lib/unicode.m31"),
        "field25519" => include_str!("../lib/field25519.m31"),
        "sha512" => include_str!("../lib/sha512.m31"),
        "x25519" => include_str!("../lib/x25519.m31"),
        "scalar25519" => include_str!("../lib/scalar25519.m31"),
        "ed25519" => include_str!("../lib/ed25519.m31"),
        "sha256" => include_str!("../lib/sha256.m31"),
        "chacha20poly1305" => include_str!("../lib/chacha20poly1305.m31"),
        "ssh" => include_str!("../lib/ssh.m31"),
        "hmac" => include_str!("../lib/hmac.m31"),
        "hkdf" => include_str!("../lib/hkdf.m31"),
        "bignum" => include_str!("../lib/bignum.m31"),
        "ecdsa" => include_str!("../lib/ecdsa.m31"),
        "x509" => include_str!("../lib/x509.m31"),
        "rsa" => include_str!("../lib/rsa.m31"),
        "tls13_schedule" => include_str!("../lib/tls13_schedule.m31"),
        "tls13_record" => include_str!("../lib/tls13_record.m31"),
        "tls" => include_str!("../lib/tls.m31"),
        _ => return None,
    })
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
