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
        "base64" => include_str!("../lib/base64.src"),
        "csv" => include_str!("../lib/csv.src"),
        "html" => include_str!("../lib/html.src"),
        "http" => include_str!("../lib/http.src"),
        "io" => include_str!("../lib/io.src"),
        "fs" => include_str!("../lib/fs.src"),
        "json" => include_str!("../lib/json.src"),
        "math" => include_str!("../lib/math.src"),
        "net" => include_str!("../lib/net.src"),
        "os" => include_str!("../lib/os.src"),
        "sort" => include_str!("../lib/sort.src"),
        "term" => include_str!("../lib/term.src"),
        "text" => include_str!("../lib/text.src"),
        "date" => include_str!("../lib/date.src"),
        "random" => include_str!("../lib/random.src"),
        "args" => include_str!("../lib/args.src"),
        "unicode" => include_str!("../lib/unicode.src"),
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
        return Some(include_str!("../lib/__floatfmt.src"));
    }
    if name == TEXT {
        return Some(include_str!("../lib/__text.src"));
    }
    source(name)
}
