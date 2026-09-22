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

/// The source of an embedded module, or `None` if the name is not one.
///
/// A module named here is taken: a file of the same name beside the program
/// is a collision, not an override, because module names are globally unique
/// (docs/modules-decision.md §1).
pub fn source(name: &str) -> Option<&'static str> {
    Some(match name {
        "base64" => include_str!("../lib/base64.src"),
        "csv" => include_str!("../lib/csv.src"),
        "html" => include_str!("../lib/html.src"),
        "io" => include_str!("../lib/io.src"),
        "json" => include_str!("../lib/json.src"),
        "math" => include_str!("../lib/math.src"),
        _ => return None,
    })
}
