use super::*;

// ---- float text ---------------------------------------------------------

/// The module that formats floats is language source the lowering calls into,
/// so a float `to_str` or `print` inside it would be a call to itself.
#[test]
fn the_float_module_cannot_format_a_float_itself() {
    let fm = crate::resolve::stdlib::FLOATFMT;
    for body in [
        "str f(float x) { return x.to_str(); }",
        "void f(float x) { print(x); }",
        "bool f(str s) { return s.parse_float().is_some(); }",
    ] {
        let toks = Lexer::new(body).tokenize().expect("lexes");
        let prog = crate::front::parser::Parser::new(toks)
            .stdlib()
            .parse_program(fm)
            .expect("parses");
        let e = match crate::finish(prog, "c") {
            Ok(_) => panic!("expected the float module to be refused: {body}"),
            Err(d) => d.to_string(),
        };
        assert!(e.contains("cannot format or parse a float itself"), "{e}");
    }
}

/// Without the module loaded, a float conversion names the missing function
/// rather than emitting a call the C compiler would reject.
#[test]
fn a_float_conversion_without_the_module_is_a_named_compiler_bug() {
    assert!(err("print(1.5);").contains("was not loaded for a float conversion"));
}

// ---- code points ----------------------------------------------------------

/// `chars` and `from_chars` are lowered to calls into lib/__text.m31, so the
/// module using either through the method spelling would call itself.
#[test]
fn the_text_module_cannot_use_its_own_conversions() {
    let tm = crate::resolve::stdlib::TEXT;
    for body in [
        "List<int> f(str s) { return s.chars(); }",
        "str f(List<int> xs) { return str.from_chars(xs); }",
    ] {
        let toks = Lexer::new(body).tokenize().expect("lexes");
        let prog = crate::front::parser::Parser::new(toks)
            .stdlib()
            .parse_program(tm)
            .expect("parses");
        let e = match crate::finish(prog, "c") {
            Ok(_) => panic!("expected the text module to be refused: {body}"),
            Err(d) => d.to_string(),
        };
        assert!(
            e.contains("cannot call `chars` or `from_chars` itself"),
            "{e}"
        );
    }
}

/// Without the module loaded, a conversion names the missing function rather
/// than emitting a call the C compiler would reject.
#[test]
fn a_code_point_conversion_without_the_module_is_a_named_compiler_bug() {
    let e = err("List<int> cs = \"a\".chars();");
    assert!(
        e.contains("was not loaded for a code point conversion"),
        "{e}"
    );
    let e = err("List<int> cs = [97]; str s = str.from_chars(cs);");
    assert!(
        e.contains("was not loaded for a code point conversion"),
        "{e}"
    );
}
