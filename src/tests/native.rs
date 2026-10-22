//! `prim "c"` (docs/ffi-decision.md): the declaration's checks and what the
//! emitter makes of a call. A declaration is library-only, so these parse as
//! standard library source (`.stdlib()`), the way the float-module tests do.

use super::*;

/// Compile `src` as library source and return the C, or the diagnostic.
fn lib(src: &str) -> Result<String, String> {
    let toks = Lexer::new(src).tokenize().expect("lexes");
    let prog = crate::front::parser::Parser::new(toks)
        .stdlib()
        .parse_program("__floatfmt")
        .map_err(|d| d.to_string())?;
    crate::finish(prog, "c").map_err(|d| d.to_string())
}

fn lib_err(src: &str) -> String {
    match lib(src) {
        Ok(_) => panic!("expected this declaration to be refused:\n{src}"),
        Err(e) => e,
    }
}

const AES: &str = "
prim \"c\" nonblocking void __k_aes_ctr(
    in bytes rk[0 .. 16 * (rounds + 1)],
    int rounds in 10 | 14,
    inout bytes ctr[0 .. 16],
    in bytes src[off .. off + 16 * n],
    out bytes dst[off .. off + 16 * n],
    int n,
    range int off);
void use_it(bytes rk, bytes ctr, bytes a, bytes b) {
    __k_aes_ctr(rk, 10, ctr, a, b, 1, 0);
}
";

#[test]
fn a_span_call_checks_every_bound_against_the_size() {
    let c = lib(AES).expect("compiles");
    assert!(c.contains("extern void rt_k_aes_ctr(const uint8_t *, int64_t, uint8_t *, const uint8_t *, uint8_t *, int64_t);"));
    // Bounds are overflow-checked, spans are checked against `len` (the
    // size, never `cap`), and only the writable spans are checked mutable.
    assert!(c.contains("__builtin_mul_overflow"), "{c}");
    assert!(c.contains("__builtin_add_overflow"), "{c}");
    assert!(c.contains("->len)"), "{c}");
    assert!(!c.contains("->cap"), "a span is checked against the size");
    assert_eq!(c.matches("rt_check_mutable(").count(), 2, "{c}");
    // `rounds` is restricted to the values the declaration lists.
    assert!(
        c.contains("!= INT64_C(10) && ") && c.contains("!= INT64_C(14)"),
        "{c}"
    );
    // `off` is a `range`: evaluated, never passed.
    assert!(c.contains("rt_k_aes_ctr(p0, v4, p2, p3, p4, v5);"), "{c}");
    // A `nonblocking` callee is not wrapped.
    assert!(
        !c.contains("rt_enter_blocking();\n        rt_k_aes_ctr"),
        "{c}"
    );
}

#[test]
fn a_target_without_kernels_gets_no_call() {
    let c = lib(AES).expect("compiles");
    assert!(c.contains("#if RT_HAVE_KERNELS"), "{c}");
    assert!(c.contains("no native kernels on this target"), "{c}");
    let c = lib("prim \"c\" int __k_probe(int a);\nint f() { return __k_probe(1); }").unwrap();
    assert!(
        c.contains("= -1;"),
        "an int callee answers -1 when absent:\n{c}"
    );
}

#[test]
fn only_a_blocking_callee_is_wrapped() {
    let src = |nb: &str| {
        format!(
            "prim \"c\" {nb} int __k_f(in bytes b[0 .. n], int n);\n\
             int g(bytes b) {{ return __k_f(b, 1); }}"
        )
    };
    let wrapped = lib(&src("")).unwrap();
    assert!(
        wrapped.contains("rt_enter_blocking();\n        v"),
        "{wrapped}"
    );
    assert!(wrapped.contains("rt_exit_blocking();"), "{wrapped}");
    let bare = lib(&src("nonblocking")).unwrap();
    assert!(!bare.contains("rt_enter_blocking"), "{bare}");
}

#[test]
fn an_array_span_is_checked_in_elements() {
    let c = lib(
        "prim \"c\" nonblocking void __k_fe(inout Array<int> out[0 .. 10], in Array<int> f[0 .. 10]);\n\
         void g(Array<int> a, Array<int> b) { __k_fe(a, b); }",
    )
    .unwrap();
    assert!(c.contains("((Arr *)v0)->len"), "{c}");
    assert!(c.contains("int64_t *p0 = ((Arr *)v0)->data"), "{c}");
    assert!(c.contains("const int64_t *p1"), "{c}");
}

#[test]
fn a_program_cannot_write_one() {
    let e = err("prim \"c\" int __k_x(int a);\nprint(1);");
    assert!(
        e.contains("only available to source the compiler ships"),
        "{e}"
    );
}

#[test]
fn the_declaration_is_checked_as_written() {
    for (body, want) in [
        ("prim \"c\" int __k_a(int x);\nint g() { return 0; }", None),
        (
            "prim \"rust\" int __k_a(int x);",
            Some("only calling convention"),
        ),
        ("prim \"c\" int __a(int x);", Some("is named `__k_...`")),
        ("prim \"c\" str __k_a(int x);", Some("returns `int`")),
        (
            "prim \"c\" void __k_a(int a, int b, int c, int d, int e, int f, int g);",
            Some("at most 6 arguments"),
        ),
        ("prim \"c\" void __k_a(bytes b);", Some("is a span")),
        (
            "prim \"c\" void __k_a(bool b);",
            Some("is an `int` or a span"),
        ),
        (
            "prim \"c\" void __k_a(in List<int> b[0 .. 1]);",
            Some("is a `bytes` or an `Array<int>`"),
        ),
        (
            "prim \"c\" void __k_a(in bytes b[0 .. m], int n);",
            Some("`m` is not a parameter"),
        ),
        (
            "prim \"c\" void __k_a(in bytes b[0 .. c], in bytes c[0 .. 1]);",
            Some("`c` is a span"),
        ),
        (
            "prim \"c\" void __k_a(in bytes b[0 .. n - 1], int n);",
            Some("expected `]`"),
        ),
        (
            "prim \"c\" void __k_a(in bytes b[0 .. n / 2], int n);",
            Some("expected `]`"),
        ),
        (
            "prim \"c\" void __k_a(int n in );",
            Some("expected an integer literal"),
        ),
    ] {
        match want {
            None => {
                lib(body).unwrap_or_else(|e| panic!("{body}: {e}"));
            }
            Some(w) => {
                let e = lib_err(body);
                assert!(e.contains(w), "{body}\n  wanted `{w}`, got:\n{e}");
            }
        }
    }
}

#[test]
fn six_passed_arguments_are_fine_and_range_does_not_count() {
    lib("prim \"c\" int __k_a(int a, int b, int c, int d, int e, int f);\nint g() { return __k_a(1, 2, 3, 4, 5, 6); }")
        .expect("six arguments");
    lib("prim \"c\" int __k_a(int a, int b, int c, int d, int e, int f, range int g);\nint h() { return __k_a(1, 2, 3, 4, 5, 6, 7); }")
        .expect("a range parameter is not an argument");
}

#[test]
fn a_call_is_type_checked_like_any_other() {
    let e = lib_err(
        "prim \"c\" int __k_f(in bytes b[0 .. n], int n);\n\
         int g(str s) { return __k_f(s, 1); }",
    );
    assert!(e.contains("bytes"), "{e}");
}

#[test]
fn the_formatter_writes_a_declaration_one_parameter_to_a_line_and_is_stable() {
    let once = crate::reformat(AES.trim_start(), "__floatfmt").expect("formats");
    assert!(once.contains(
        "prim \"c\" nonblocking void __k_aes_ctr(\n    in bytes rk[0 .. 16 * (rounds + 1)],\n    int rounds in 10 | 14,"
    ), "{once}");
    assert!(once.contains("    range int off);\n"), "{once}");
    let twice = crate::reformat(&once, "__floatfmt").expect("formats again");
    assert_eq!(once, twice);
}
