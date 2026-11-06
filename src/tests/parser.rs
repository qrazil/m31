use super::*;

#[test]
fn arithmetic_precedence_matches_c() {
    // 2 + 3 * 4 must multiply first: the imul has to consume the constants,
    // and the iadd has to consume the imul's result.
    let out = ir("print(2 + 3 * 4);");
    let mul = out.find("imul").expect("expected a multiply");
    let add = out.find("iadd").expect("expected an add");
    assert!(mul < add, "multiply must be emitted before add:\n{out}");
}

#[test]
fn comparison_binds_looser_than_arithmetic() {
    let out = ir("print(1 + 2 < 4);");
    let add = out.find("iadd").expect("expected an add");
    let cmp = out.find("icmp").expect("expected a compare");
    assert!(add < cmp, "add must be emitted before compare:\n{out}");
}

#[test]
fn bit_operators_bind_tighter_than_comparison() {
    // Python's order, not C's: `x & 1 == 0` is `(x & 1) == 0`.
    let out = ir("int x = 6; print(x & 1 == 0);");
    let and = out.find("iand").expect("expected a bitwise and");
    let cmp = out.find("icmp").expect("expected a compare");
    assert!(and < cmp, "and must be emitted before compare:\n{out}");
}

#[test]
fn shifts_bind_looser_than_addition() {
    let out = ir("int x = 1; print(x << x + 1);");
    let add = out.find("iadd").expect("expected an add");
    let shl = out.find("ishl").expect("expected a shift");
    assert!(add < shl, "add must be emitted before shift:\n{out}");
}

#[test]
fn right_shift_needs_adjacent_angle_brackets() {
    assert!(err("int x = 8; print(x > > 1);").contains("expected an expression"));
}

// ---- docstrings -----------------------------------------------------------

fn parse_src(src: &str) -> Result<crate::front::ast::Program, crate::front::diag::Diag> {
    let toks = Lexer::new(src).tokenize()?;
    crate::front::parser::Parser::new(toks).parse_program("sample")
}

#[test]
fn docstrings_attach_to_every_kind_of_declaration() {
    let src = "/// Module constant.\npub const int LIMIT = 3;\n\
/// A shape.\npub type Shape {\n    /// Its side.\n    pub int side;\n}\n\
/// Colours.\npub enum Colour {\n    /// The warm one.\n    Red;\n    Blue;\n}\n\
/// Has area.\npub interface Area {\n    /// How big.\n    int area();\n}\n\
/// Doubles.\npub int twice(int value) {\n    return value * 2;\n}\n\
/** A\n * block. */\nint counter = 0;\n";
    let p = parse_src(src).expect("parses");
    assert_eq!(p.consts[0].doc.as_deref(), Some("Module constant."));
    assert_eq!(p.types[0].doc.as_deref(), Some("A shape."));
    assert_eq!(p.types[0].fields[0].doc.as_deref(), Some("Its side."));
    assert_eq!(p.types[1].doc.as_deref(), Some("Colours."));
    assert_eq!(p.types[1].variants[0].doc.as_deref(), Some("The warm one."));
    assert_eq!(p.types[1].variants[1].doc, None);
    assert_eq!(p.types[2].methods[0].doc.as_deref(), Some("How big."));
    assert_eq!(p.funcs[0].doc.as_deref(), Some("Doubles."));
    assert!(matches!(
        &p.toplevel[0],
        crate::front::ast::Stmt::Decl { doc: Some(d), .. } if d == "A\nblock."
    ));
}

#[test]
fn a_doc_comment_before_anything_else_is_refused() {
    for src in [
        "/// a\nprint(1);\n",
        "int one = 1;\n/// a\n",
        "int one = 1;\n/// a\n\nint two = 2;\n",
        "import io;\n/// a\n",
        "/// a\nimport io;\n",
        "pub int fine() {\n    /// a\n    return 1;\n}\n",
        "type P {\n    int x;\n    /// last\n}\n",
        "int total(\n/// a\nint v) {\n    return v;\n}\n",
    ] {
        let e = parse_src(src).expect_err(src).to_string();
        assert!(
            e.contains("doc comment is not attached to a declaration"),
            "{src}: {e}"
        );
    }
}
