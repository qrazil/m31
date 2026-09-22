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
        "io" => include_str!("../lib/io.src"),
        "math" => include_str!("../lib/math.src"),
        "os" => include_str!("../lib/os.src"),
        "date" => include_str!("../lib/date.src"),
        "random" => include_str!("../lib/random.src"),
        _ => return None,
    })
}
