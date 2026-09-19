//! Unit tests.
//!
//! These complement the corpus rather than duplicating it. The corpus proves
//! that compiled programs BEHAVE correctly, end to end, against four C
//! builds and a Go twin. These prove the compiler's internals: that the lexer
//! rejects what it should, and that lowering produces the IR we think it
//! does.
//!
//! The IR snapshots matter because the corpus cannot see them. A lowering bug
//! that happens to produce correct output -- a redundant retain/release pair,
//! a block that is never reached, an extra temporary -- is invisible at the
//! C level and obvious here.

use crate::{compile, lexer::Lexer, lexer::Tok};

fn ir(src: &str) -> String {
    compile(src, "ir").expect("expected this program to compile")
}

fn err(src: &str) -> String {
    match compile(src, "c") {
        Ok(_) => panic!("expected this program to be rejected"),
        Err(d) => d.to_string(),
    }
}

fn toks(src: &str) -> Vec<Tok> {
    Lexer::new(src)
        .tokenize()
        .expect("expected this to lex")
        .into_iter()
        .map(|t| t.tok)
        .collect()
}

// ---- lexer ----------------------------------------------------------

#[test]
fn lexes_two_character_operators_greedily() {
    assert_eq!(
        toks("== = != ! <= < >= > && ||"),
        vec![
            Tok::EqEq,
            Tok::Assign,
            Tok::BangEq,
            Tok::Bang,
            Tok::LtEq,
            Tok::Lt,
            Tok::GtEq,
            Tok::Gt,
            Tok::AmpAmp,
            Tok::PipePipe,
            Tok::Eof,
        ]
    );
}

#[test]
fn underscores_in_int_literals_are_separators() {
    assert_eq!(toks("1_000_000"), vec![Tok::Int(1_000_000), Tok::Eof]);
}

#[test]
fn string_literals_keep_multibyte_utf8_intact() {
    // Regression: accumulating `byte as char` decoded each UTF-8
    // continuation byte as its own Latin-1 codepoint and mangled the string.
    assert_eq!(
        toks(r#""héllo →""#),
        vec![Tok::Str("héllo →".into()), Tok::Eof]
    );
}

#[test]
fn string_escapes_are_decoded() {
    assert_eq!(
        toks(r#""a\tb\nc\\d\"e""#),
        vec![Tok::Str("a\tb\nc\\d\"e".into()), Tok::Eof]
    );
}

#[test]
fn lexer_rejects_bad_input() {
    let bad = |src: &str| Lexer::new(src).tokenize().is_err();
    assert!(bad(r#""unterminated"#), "unterminated string");
    assert!(bad("\"has a\nnewline\""), "newline inside string");
    assert!(bad(r#""\q""#), "unknown escape");
    assert!(bad("/* unterminated"), "unterminated block comment");
    assert!(bad("123abc"), "suffix on integer");
    assert!(
        bad("9223372036854775808"),
        "integer literal too large for int"
    );
    assert!(bad("a & b"), "single ampersand");
    assert!(bad("a @ b"), "unexpected character");
}

#[test]
fn comments_are_trivia() {
    assert_eq!(
        toks("1 // line\n/* block */ 2"),
        vec![Tok::Int(1), Tok::Int(2), Tok::Eof]
    );
}

// ---- parser ---------------------------------------------------------

#[test]
fn arithmetic_precedence_matches_c() {
    // 2 + 3 * 4 must multiply first: the imul has to consume the constants,
    // and the iadd has to consume the imul's result.
    let out = ir("void main() { print(2 + 3 * 4); }");
    let mul = out.find("imul").expect("expected a multiply");
    let add = out.find("iadd").expect("expected an add");
    assert!(mul < add, "multiply must be emitted before add:\n{out}");
}

#[test]
fn comparison_binds_looser_than_arithmetic() {
    let out = ir("void main() { print(1 + 2 < 4); }");
    let add = out.find("iadd").expect("expected an add");
    let cmp = out.find("icmp").expect("expected a compare");
    assert!(add < cmp, "add must be emitted before compare:\n{out}");
}

// ---- lowering: refcounting -------------------------------------------

#[test]
fn borrowed_argument_causes_no_refcount_traffic() {
    // docs/ir-v0.md §5.1: arguments are borrowed. Passing a value a caller
    // already holds to a function that only reads it must cost nothing.
    let out =
        ir("int take(str s) { return len(s); }\nvoid main() { str s = \"hi\"; print(take(s)); }");
    let take = out.split("func main").next().unwrap();
    assert!(
        !take.contains("rc_inc") && !take.contains("rc_dec"),
        "callee must not touch the refcount of a borrowed argument:\n{take}"
    );
}

#[test]
fn owned_call_result_is_not_retained_again() {
    // concat returns +1. Binding it to a local must NOT add a second retain;
    // the local takes the existing one over.
    let out = ir("void main() { str c = concat(\"a\", \"b\"); print(c); }");
    assert_eq!(
        out.matches("rc_inc").count(),
        0,
        "an owned result must be moved into the local, not retained:\n{out}"
    );
    assert_eq!(
        out.matches("rc_dec").count(),
        1,
        "the local must be released exactly once:\n{out}"
    );
}

#[test]
fn borrowed_source_is_retained_when_bound() {
    // Binding one local from another creates a second reference, so the new
    // local must retain, and both must release.
    let out = ir("void main() { str a = concat(\"a\", \"b\"); str b = a; print(b); }");
    assert_eq!(
        out.matches("rc_inc").count(),
        1,
        "second binding must retain:\n{out}"
    );
    assert_eq!(
        out.matches("rc_dec").count(),
        2,
        "both locals must release:\n{out}"
    );
}

#[test]
fn returning_a_borrowed_local_retains_before_release() {
    // Returns are owned (+1). Returning a local must retain it before the
    // scope releases it, or the caller receives a freed object.
    let out = ir("str f() { str s = \"x\"; return s; }\nvoid main() { print(f()); }");
    let f = out.split("func main").next().unwrap();
    let inc = f.find("rc_inc").expect("return of a local must retain");
    let dec = f.find("rc_dec").expect("scope must still release");
    assert!(inc < dec, "retain must precede release:\n{f}");
}

// ---- lowering: control flow ------------------------------------------

#[test]
fn if_merge_introduces_a_block_parameter() {
    // Both arms reassign x, so the join block carries it as a parameter.
    let out = ir("void main() { int x = 1; if (x > 0) { x = 2; } else { x = 3; } print(x); }");
    assert!(
        out.contains("block3(v") || out.contains("block2(v") || out.contains("block1(v"),
        "expected a join block with a parameter:\n{out}"
    );
}

#[test]
fn short_circuit_and_is_lowered_as_branches() {
    let out = ir("void main() { if (true && false) { print(1); } }");
    assert!(
        out.contains("brif"),
        "&& must lower to control flow:\n{out}"
    );
}

#[test]
fn unary_minus_goes_through_checked_subtraction() {
    // Lowering -x as 0 - x is what makes -INT64_MIN trap instead of wrapping.
    let out = ir("void main() { int x = 1; print(-x); }");
    assert!(
        out.contains("isub"),
        "negation must use checked subtraction:\n{out}"
    );
}

// ---- diagnostics ------------------------------------------------------

#[test]
fn rejects_type_errors() {
    assert_eq!(
        err("void main() { int a = \"s\"; print(a); }"),
        "1:23: type mismatch: expected int, found str"
    );
    assert_eq!(
        err("void main() { print(nope); }"),
        "1:21: unknown variable `nope`"
    );
    assert_eq!(
        err("void main() { if (1) { print(1); } }"),
        "1:19: type mismatch: expected bool, found int"
    );
}

#[test]
fn rejects_malformed_programs() {
    // Needs a `main`, because the missing-`main` check runs first.
    assert!(err("int f() { }\nvoid main() { }").contains("must return a value"));
    assert!(err("void main() { return 1; }").contains("cannot return a value"));
    assert!(err("void f() { }").contains("no `main`"));
    assert!(err("void main(int x) { }").contains("takes no parameters"));
    assert!(err("int main() { return 0; }").contains("must return `void`"));
    assert!(err("void main() { int a = 1; int a = 2; print(a); }").contains("already declared"));
    assert!(err("void f() { } void f() { } void main() { }").contains("already defined"));
    assert!(err("void main() { int a = 1; a = \"s\"; print(a); }").contains("type mismatch"));
}

#[test]
fn bool_supports_only_equality() {
    assert!(err("void main() { print(true < false); }").contains("only `==` and `!=`"));
}

#[test]
fn str_cannot_be_compared_or_added() {
    assert!(err("void main() { str a = \"x\"; print(a + a); }").contains("cannot apply `+`"));
    assert!(err("void main() { str a = \"x\"; print(a == a); }").contains("cannot compare"));
}
