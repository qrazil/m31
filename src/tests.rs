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

// ---- lowering: loops --------------------------------------------------

#[test]
fn while_creates_a_header_with_loop_carried_parameters() {
    // The header dominates its own body, so its parameters must exist before
    // the body is lowered. Both i and total are carried.
    let out = ir(
        "void main() { int i = 0; int t = 0; while (i < 3) { t = t + i; i = i + 1; } print(t); }",
    );
    assert!(
        out.contains("block1(v") && out.contains("jump block1("),
        "expected a parameterised loop header with a back edge:\n{out}"
    );
    // Two carried variables means two header parameters.
    let header = out
        .lines()
        .find(|l| l.starts_with("block1("))
        .expect("no header line");
    assert_eq!(
        header.matches(',').count(),
        1,
        "expected exactly two header parameters, got: {header}"
    );
}

#[test]
fn loop_body_declarations_are_not_loop_carried() {
    // `b` is declared inside the body, so it is fresh each iteration and must
    // NOT become a header parameter.
    let out =
        ir("void main() { int a = 0; while (a < 3) { int b = a; a = a + b + 1; } print(a); }");
    let header = out
        .lines()
        .find(|l| l.starts_with("block1("))
        .expect("no header line");
    assert!(
        !header.contains(","),
        "only `a` should be carried, got: {header}"
    );
}

#[test]
fn loop_reassigning_a_str_releases_the_previous_value() {
    // Without the release, every iteration leaks. The corpus proves this at
    // runtime via __rc_live; this proves the instruction is emitted at all.
    let out = ir(
        "void main() { str s = \"\"; int i = 0; while (i < 2) { s = concat(s, \"x\"); i = i + 1; } print(s); }",
    );
    assert!(
        out.matches("rc_dec").count() >= 2,
        "expected a release inside the loop and one at scope end:\n{out}"
    );
}

#[test]
fn break_releases_locals_declared_in_the_loop_body() {
    // The break path leaves the body scope, so it must release what the body
    // allocated -- release_to_depth exists for exactly this.
    let out = ir(
        "void main() { int i = 0; while (i < 3) { str t = concat(\"a\",\"b\"); if (len(t) == 2) { break; } i = i + 1; } print(i); }",
    );
    // One release on the break path, one at the normal end of the iteration.
    assert!(
        out.matches("rc_dec").count() >= 2,
        "break must release body-scope locals:\n{out}"
    );
}

#[test]
fn break_and_continue_outside_a_loop_are_rejected() {
    assert!(err("void main() { break; }").contains("`break` outside a loop"));
    assert!(err("void main() { continue; }").contains("`continue` outside a loop"));
}

#[test]
fn break_makes_the_exit_block_a_merge_point() {
    // Without break the exit needs no parameters. With it, the exit merges the
    // header's values with the break site's, so it must take parameters.
    let out =
        ir("void main() { int i = 0; int f = 0; while (i < 9) { if (i > 3) { f = i; break; } i = i + 1; } print(f); }");
    let exit_has_params = out
        .lines()
        .filter(|l| l.starts_with("block") && l.contains("(v") && l.ends_with("):"))
        .count();
    assert!(
        exit_has_params >= 2,
        "expected both a parameterised header and a parameterised exit:\n{out}"
    );
}

// ---- user types -------------------------------------------------------

#[test]
fn a_type_with_no_references_needs_no_drop() {
    // rc_dec checks drop for NULL, so a type holding no references pays a
    // predictable branch instead of a call.
    let out = ir("type P { int x; }\nvoid main() { P p = P(x: 1); print(p.x); }");
    assert!(
        out.contains("alloc type0"),
        "expected an allocation:\n{out}"
    );
}

#[test]
fn construction_retains_a_borrowed_field_but_not_an_owned_one() {
    // concat returns +1 and is handed straight over; a local is borrowed and
    // must be retained, because the object now holds a reference too.
    let owned =
        ir("type B { str s; }\nvoid main() { B b = B(s: concat(\"a\",\"b\")); print(b.s); }");
    assert_eq!(
        owned.matches("rc_inc").count(),
        0,
        "an owned value must be moved into the field:\n{owned}"
    );

    let borrowed =
        ir("type B { str s; }\nvoid main() { str t = \"x\"; B b = B(s: t); print(b.s); }");
    assert!(
        borrowed.contains("rc_inc"),
        "a borrowed value must be retained when stored:\n{borrowed}"
    );
}

#[test]
fn assigning_a_reference_field_releases_the_old_value() {
    // Load old, retain new, store, release old -- in that order, so
    // `p.f = p.f;` cannot free what it is assigning.
    let out = ir(
        "type B { str s; }\nvoid main() { B b = B(s: \"a\"); b.s = concat(\"c\",\"d\"); print(b.s); }",
    );
    // Compare against the LAST store, not the first: the first store belongs
    // to the construction above, which legitimately precedes any load.
    let lines: Vec<&str> = out.lines().collect();
    let load = lines
        .iter()
        .position(|l| l.contains("load"))
        .expect("must read the old value");
    let last_store = lines
        .iter()
        .rposition(|l| l.contains("store"))
        .expect("must store the new one");
    assert!(
        load < last_store,
        "old value must be read before the assigning store:\n{out}"
    );
    assert!(
        out.matches("rc_dec").count() >= 2,
        "old field value and the object itself must both be released:\n{out}"
    );
}

#[test]
fn field_reads_are_borrowed() {
    // Reading a field does not take a reference: the object holds the +1.
    let out = ir("type B { str s; }\nvoid main() { B b = B(s: \"a\"); print(b.s); }");
    // One release for b at scope end, and nothing extra for the read.
    assert_eq!(
        out.matches("rc_dec").count(),
        1,
        "a field read must not add refcount traffic:\n{out}"
    );
}

#[test]
fn const_locals_reject_assignment() {
    assert!(err("void main() { const int x = 1; x = 2; print(x); }")
        .contains("cannot assign to const `x`"));
}

#[test]
fn type_errors_on_user_types_are_reported() {
    assert!(
        err("type P { int x; }\nvoid main() { P p = P(x: \"s\"); print(p.x); }")
            .contains("field `x` is int, found str")
    );
    assert!(
        err("type P { int x; }\nvoid main() { P p = P(); print(p.x); }")
            .contains("missing field `x`")
    );
    assert!(
        err("type P { int x; }\nvoid main() { P p = P(x: 1, x: 2); print(p.x); }")
            .contains("given twice")
    );
    assert!(err("type P { int x; } type P { int y; }\nvoid main() { }").contains("already defined"));
    assert!(err("void main() { int i = 1; print(i.x); }").contains("has no fields"));
}

// ---- generics ---------------------------------------------------------

#[test]
fn generics_are_erased_before_lowering() {
    // The IR must contain no type parameters at all -- only instantiations.
    let out = ir("type Box<T> { T value; }\nvoid main() { Box<int> b = Box<int>(value: 1); print(b.value); }");
    assert!(
        out.contains("type Box$int"),
        "expected an instantiation:\n{out}"
    );
    assert!(
        !out.contains("<T>"),
        "no type parameter may survive:\n{out}"
    );
}

#[test]
fn each_distinct_instantiation_is_emitted_once() {
    let out = ir(
        "type Box<T> { T value; }\nvoid main() { Box<int> a = Box<int>(value: 1); Box<int> b = Box<int>(value: 2); Box<str> c = Box<str>(value: \"s\"); print(a.value + b.value); print(c.value); }",
    );
    assert_eq!(
        out.matches("type Box$int").count(),
        1,
        "Box<int> must be instantiated exactly once:\n{out}"
    );
    assert!(out.contains("type Box$str"), "Box<str> missing:\n{out}");
}

#[test]
fn an_unused_generic_is_never_instantiated() {
    // Instantiation is a worklist over the reachable set, so an unused
    // generic is never type-checked against types it was not written for.
    let out = ir("type Unused<T> { T v; }\nvoid main() { print(1); }");
    assert!(
        !out.contains("Unused"),
        "an unused generic must not be emitted:\n{out}"
    );
}

#[test]
fn generic_function_type_arguments_are_inferred() {
    let out = ir(
        "type Box<T> { T value; }\nT unwrap<T>(Box<T> b) { return b.value; }\nvoid main() { Box<int> b = Box<int>(value: 1); print(unwrap(b)); }",
    );
    assert!(
        out.contains("func unwrap$int"),
        "expected the inferred instantiation:\n{out}"
    );
}

#[test]
fn generic_misuse_is_rejected() {
    assert!(
        err("type Box<T> { T value; }\nvoid main() { Box<int,str> b = Box<int,str>(value: 1); print(b.value); }")
            .contains("takes 1 type argument(s), found 2")
    );
    assert!(
        err("type P { int x; }\nvoid main() { P<int> p = P<int>(x: 1); print(p.x); }")
            .contains("is not generic")
    );
    // Inference cannot see through a nested call; the diagnostic says what to
    // do rather than guessing.
    assert!(err(
        "type Box<T> { T value; }\nT unwrap<T>(Box<T> b) { return b.value; }\nvoid main() { Box<Box<int>> n = Box<Box<int>>(value: Box<int>(value: 1)); print(unwrap(unwrap(n))); }"
    )
    .contains("bind the argument to a local"));
}
