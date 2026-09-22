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

use crate::{compile_str, lexer::Lexer, lexer::Tok};

fn ir(src: &str) -> String {
    compile_str(src, "ir").expect("expected this program to compile")
}

fn err(src: &str) -> String {
    match compile_str(src, "c") {
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

// ---- lowering: refcounting -------------------------------------------

#[test]
fn borrowed_argument_causes_no_refcount_traffic() {
    // docs/ir-v0.md §5.1: arguments are borrowed. Passing a value a caller
    // already holds to a function that only reads it must cost nothing.
    let out = ir("int take(str s) { return s.size(); }\nstr s = \"hi\"; print(take(s));");
    let take = out.split("func $main").next().unwrap();
    assert!(
        !take.contains("rc_inc") && !take.contains("rc_dec"),
        "callee must not touch the refcount of a borrowed argument:\n{take}"
    );
}

#[test]
fn owned_call_result_is_not_retained_again() {
    // concat returns +1. Binding it to a local must NOT add a second retain;
    // the local takes the existing one over.
    let out = ir("str c = concat(\"a\", \"b\"); print(c);");
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
    let out = ir("str a = concat(\"a\", \"b\"); str b = a; print(b);");
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
    let out = ir("str f() { str s = \"x\"; return s; }\nprint(f());");
    let f = out.split("func $main").next().unwrap();
    let inc = f.find("rc_inc").expect("return of a local must retain");
    let dec = f.find("rc_dec").expect("scope must still release");
    assert!(inc < dec, "retain must precede release:\n{f}");
}

// ---- lowering: control flow ------------------------------------------

#[test]
fn if_merge_introduces_a_block_parameter() {
    // Both arms reassign x, so the join block carries it as a parameter.
    let out = ir("int x = 1; if (x > 0) { x = 2; } else { x = 3; } print(x);");
    assert!(
        out.contains("block3(v") || out.contains("block2(v") || out.contains("block1(v"),
        "expected a join block with a parameter:\n{out}"
    );
}

#[test]
fn short_circuit_and_is_lowered_as_branches() {
    let out = ir("if (true && false) { print(1); }");
    assert!(
        out.contains("brif"),
        "&& must lower to control flow:\n{out}"
    );
}

#[test]
fn unary_minus_goes_through_checked_subtraction() {
    // Lowering -x as 0 - x is what makes -INT64_MIN trap instead of wrapping.
    let out = ir("int x = 1; print(-x);");
    assert!(
        out.contains("isub"),
        "negation must use checked subtraction:\n{out}"
    );
}

// ---- diagnostics ------------------------------------------------------

#[test]
fn rejects_type_errors() {
    assert_eq!(
        err("int a = \"s\"; print(a);"),
        "1:9: type mismatch: expected int, found str"
    );
    assert_eq!(err("print(nope);"), "1:7: unknown variable `nope`");
    assert_eq!(
        err("if (1) { print(1); }"),
        "1:5: type mismatch: expected bool, found int"
    );
}

#[test]
fn rejects_malformed_programs() {
    assert!(err("int f() { }\nprint(1);").contains("must return a value"));
    // `return` at the top level: the program's body returns nothing.
    assert!(err("return 1;").contains("cannot return a value"));
    // Writing `main` is a habit from C, Java and Go, and without a
    // diagnostic it would declare a function nothing calls and the program
    // would silently do nothing.
    assert!(err("void main() { print(1); }").contains("there is no `main`"));
    assert!(err("int main() { return 0; }").contains("there is no `main`"));
    assert!(err("int a = 1; int a = 2; print(a);").contains("shadowing is not allowed"));
    assert!(err("void f() { } void f() { } print(1);").contains("already defined"));
    assert!(err("void f() { } void f() { } print(1);").contains("already defined"));
    assert!(err("int a = 1; a = \"s\"; print(a);").contains("type mismatch"));
}

#[test]
fn bool_supports_only_equality() {
    assert!(err("print(true < false);").contains("only `==` and `!=`"));
}

#[test]
fn str_supports_only_plus_and_equality() {
    // `+` and `==` are built in; nothing else is.
    assert!(err("str a = \"x\"; print(a - a);").contains("cannot apply `-`"));
    assert!(err("str a = \"x\"; print(a < a);").contains("cannot compare"));
}

// ---- lowering: loops --------------------------------------------------

#[test]
fn while_creates_a_header_with_loop_carried_parameters() {
    // The header dominates its own body, so its parameters must exist before
    // the body is lowered. Both i and total are carried.
    let out = ir("int i = 0; int t = 0; while (i < 3) { t = t + i; i = i + 1; } print(t);");
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
    let out = ir("int a = 0; while (a < 3) { int b = a; a = a + b + 1; } print(a);");
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
    let out =
        ir("str s = \"\"; int i = 0; while (i < 2) { s = concat(s, \"x\"); i = i + 1; } print(s);");
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
        "int i = 0; while (i < 3) { str t = concat(\"a\",\"b\"); if (t.size() == 2) { break; } i = i + 1; } print(i);",
    );
    // One release on the break path, one at the normal end of the iteration.
    assert!(
        out.matches("rc_dec").count() >= 2,
        "break must release body-scope locals:\n{out}"
    );
}

#[test]
fn break_and_continue_outside_a_loop_are_rejected() {
    assert!(err("break;").contains("`break` outside a loop"));
    assert!(err("continue;").contains("`continue` outside a loop"));
}

#[test]
fn break_makes_the_exit_block_a_merge_point() {
    // Without break the exit needs no parameters. With it, the exit merges the
    // header's values with the break site's, so it must take parameters.
    let out = ir(
        "int i = 0; int f = 0; while (i < 9) { if (i > 3) { f = i; break; } i = i + 1; } print(f);",
    );
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
    // Not `alloc type0`: the compiler always declares List$str and
    // Option$int, so a user type's index is not zero and asserting on it
    // would be testing the numbering rather than the property.
    let out = ir("type P { int x; }\nP p = P(1); print(p.x);");
    assert!(out.contains("type P { x: I64 }"), "expected P:\n{out}");
    assert!(out.contains(" = alloc "), "expected an allocation:\n{out}");
}

#[test]
fn construction_retains_a_borrowed_field_but_not_an_owned_one() {
    // concat returns +1 and is handed straight over; a local is borrowed and
    // must be retained, because the object now holds a reference too.
    let owned = ir("type B { str s; }\nB b = B(concat(\"a\",\"b\")); print(b.s);");
    assert_eq!(
        owned.matches("rc_inc").count(),
        0,
        "an owned value must be moved into the field:\n{owned}"
    );

    let borrowed = ir("type B { str s; }\nstr t = \"x\"; B b = B(t); print(b.s);");
    assert!(
        borrowed.contains("rc_inc"),
        "a borrowed value must be retained when stored:\n{borrowed}"
    );
}

#[test]
fn assigning_a_reference_field_releases_the_old_value() {
    // Load old, retain new, store, release old -- in that order, so
    // `p.f = p.f;` cannot free what it is assigning.
    let out = ir("type B { str s; }\nB b = B(\"a\"); b.s = concat(\"c\",\"d\"); print(b.s);");
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
    let out = ir("type B { str s; }\nB b = B(\"a\"); print(b.s);");
    // One release for b at scope end, and nothing extra for the read.
    assert_eq!(
        out.matches("rc_dec").count(),
        1,
        "a field read must not add refcount traffic:\n{out}"
    );
}

#[test]
fn const_locals_reject_assignment() {
    assert!(err("const int x = 1; x = 2; print(x);").contains("cannot assign to const `x`"));
}

#[test]
fn type_errors_on_user_types_are_reported() {
    assert!(err("type P { int x; }\nP p = P(\"s\"); print(p.x);")
        .contains("field `x` is int, found str"));
    assert!(err("type P { int x; }\nP p = P(); print(p.x);")
        .contains("takes 1 positional argument(s), found 0"));
    assert!(err("type P { int x; }\nP p = P(1, x: 2); print(p.x);").contains("is mandatory"));
    assert!(err("type P { int x; } type P { int y; }\nprint(1);").contains("already defined"));
    assert!(err("int i = 1; print(i.x);").contains("has no fields"));
}

// ---- generics ---------------------------------------------------------

#[test]
fn generics_are_erased_before_lowering() {
    // The IR must contain no type parameters at all -- only instantiations.
    let out = ir("type Box<T> { T value; }\nBox<int> b = Box<int>(1); print(b.value);");
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
        "type Box<T> { T value; }\nBox<int> a = Box<int>(1); Box<int> b = Box<int>(2); Box<str> c = Box<str>(\"s\"); print(a.value + b.value); print(c.value);",
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
    let out = ir("type Unused<T> { T v; }\nprint(1);");
    assert!(
        !out.contains("Unused"),
        "an unused generic must not be emitted:\n{out}"
    );
}

#[test]
fn generic_function_type_arguments_are_inferred() {
    let out = ir(
        "type Box<T> { T value; }\nT unwrap<T>(Box<T> b) { return b.value; }\nBox<int> b = Box<int>(1); print(unwrap(b));",
    );
    assert!(
        out.contains("func unwrap$int"),
        "expected the inferred instantiation:\n{out}"
    );
}

#[test]
fn generic_misuse_is_rejected() {
    assert!(
        err("type Box<T> { T value; }\nBox<int,str> b = Box<int,str>(1); print(b.value);")
            .contains("takes 1 type argument(s), found 2")
    );
    assert!(err("type P { int x; }\nP<int> p = P<int>(1); print(p.x);").contains("is not generic"));
    // Inference cannot see through a nested call; the diagnostic says what to
    // do rather than guessing.
    assert!(err(
        "type Box<T> { T value; }\nT unwrap<T>(Box<T> b) { return b.value; }\nBox<Box<int>> n = Box<Box<int>>(Box<int>(1)); print(unwrap(unwrap(n)));"
    )
    .contains("bind the argument to a local"));
}

// ---- arguments --------------------------------------------------------

#[test]
fn mandatory_is_positional_and_optional_is_named() {
    let out = ir("int f(int a, int b = 7) { return a + b; }\nprint(f(1));\nprint(f(1, b: 2));");
    assert!(out.contains("func f"), "expected the function:\n{out}");
}

#[test]
fn defaults_are_filled_in_at_the_call_site() {
    // The default is an expression evaluated at the call, so the constant
    // appears in the caller rather than the callee.
    let out = ir("int f(int a, int b = 7) { return a + b; }\nprint(f(1));");
    let caller = out.split("func $main").nth(1).unwrap();
    assert!(
        caller.contains("iconst 7"),
        "the default must be materialised by the caller:\n{caller}"
    );
}

#[test]
fn the_argument_rule_is_enforced() {
    // Naming a mandatory parameter is the mistake someone arriving from
    // Python makes, and it gets the specific message rather than an arity one.
    assert!(err("int f(int a) { return a; }\nprint(f(a: 1));")
        .contains("`a` is mandatory, so it is positional"));
    assert!(
        err("int f(int a, int b = 1) { return a; }\nprint(f(1, 2));")
            .contains("takes 1 positional argument(s), found 2")
    );
    assert!(
        err("int f(int a, int b = 1) { return a; }\nprint(f(1, c: 2));")
            .contains("has no parameter `c`")
    );
    assert!(
        err("int f(int a, int b = 1) { return a; }\nprint(f(b: 2, 1));")
            .contains("positional arguments must come before named ones")
    );
    assert!(
        err("int f(int a, int b = 1) { return a; }\nprint(f(1, b: 2, b: 3));")
            .contains("given twice")
    );
}

#[test]
fn construction_uses_the_same_rule() {
    let out = ir("type P { int x; str tag = \"none\"; }\nP p = P(1);\nprint(p.tag);");
    assert!(out.contains(" = alloc "), "expected construction:\n{out}");
    assert!(err("type P { int x; }\nP p = P(x: 1);\nprint(p.x);")
        .contains("`x` is mandatory, so it is positional"));
}

#[test]
fn type_names_need_no_capital() {
    // Recognition is by declaration, not by spelling: the parser collects
    // every `type IDENT` in a pre-pass. Capitalisation is a convention the
    // language does not enforce.
    let out = ir("type point { int x; }\npoint p = point(1);\nprint(p.x);");
    assert!(out.contains("type point"), "expected the type:\n{out}");
}

#[test]
fn a_function_may_not_share_a_name_with_a_type() {
    // A type wins in construction position, so the function would be
    // silently unreachable -- and with no capitalisation rule, the collision
    // is easy to hit by accident.
    assert!(
        err("type foo { int x; }\nint foo(int n) { return n; }\nprint(1);")
            .contains("`foo` is already a type")
    );
}

// ---- methods and shadowing --------------------------------------------

#[test]
fn methods_are_declared_by_qualified_name() {
    let out = ir("type R { int w; }\nint R.area() { return w; }\nR r = R(2); print(r.area());");
    assert!(out.contains("func R.area"), "expected the method:\n{out}");
}

#[test]
fn a_bare_name_in_a_method_reads_the_field() {
    // No `this`: the field is reached bare, and borrowed from the receiver.
    let out = ir(
        "type R { int w; int h; }\nint R.area() { return w * h; }\nR r = R(2,3); print(r.area());",
    );
    let m = out.split("func $main").next().unwrap();
    assert_eq!(
        m.matches("load").count(),
        2,
        "both fields must be read from the receiver:\n{m}"
    );
}

#[test]
fn a_method_may_assign_a_field_by_bare_name() {
    let out =
        ir("type R { int w; }\nvoid R.grow() { w = w + 1; }\nR r = R(1); r.grow(); print(r.w);");
    let m = out.split("func $main").next().unwrap();
    assert!(m.contains("store"), "expected a field store:\n{m}");
}

#[test]
fn the_receiver_is_borrowed_like_any_argument() {
    let out = ir("type R { int w; }\nint R.get() { return w; }\nR r = R(1); print(r.get());");
    let m = out.split("func $main").next().unwrap();
    assert!(
        !m.contains("rc_inc") && !m.contains("rc_dec"),
        "a method must not touch its receiver's refcount:\n{m}"
    );
}

#[test]
fn nothing_shadows_anything() {
    let msg = "shadowing is not allowed";
    // an outer local
    assert!(err("int x = 1; if (true) { int x = 2; print(x); } print(x);").contains(msg));
    // a parameter
    assert!(err("int f(int a) { int a = 2; return a; }\nprint(f(1));").contains(msg));
    // a loop's enclosing local
    assert!(err("int i = 0; while (i < 3) { int i = 9; print(i); }").contains(msg));
    // a function
    assert!(err("int helper() { return 1; }\nint helper = 5; print(helper);").contains(msg));
    // a field of the receiver
    assert!(
        err("type R { int w; }\nint R.bad() { int w = 5; return w; }\nprint(1);").contains(msg)
    );
}

#[test]
fn sibling_scopes_may_reuse_a_name() {
    // Not shadowing: neither is visible to the other.
    let out = ir(
        "int t = 0; if (true) { int s = 1; t = s; } if (t > 0) { int s = 2; t = t + s; } print(t);",
    );
    assert!(out.contains("func $main"), "expected it to compile:\n{out}");
}

// ---- operator overloading ---------------------------------------------

#[test]
fn operators_desugar_to_methods() {
    let src = "type M { int c; }\nM M.add(M o) { return M(c + o.c); }\nM a = M(1); M b = M(2); print((a + b).c);";
    let out = ir(src);
    assert!(
        out.contains("call M.add"),
        "`+` must call the method:\n{out}"
    );
}

#[test]
fn comparison_goes_through_a_single_cmp() {
    // One implementation gives a total order; four separate methods could be
    // made inconsistent with each other.
    let src = "type M { int c; }\nint M.cmp(M o) { return c - o.c; }\nM a = M(1); M b = M(2); print(a < b); print(a >= b);";
    let out = ir(src);
    assert_eq!(
        out.matches("call M.cmp").count(),
        2,
        "both comparisons go through cmp:\n{out}"
    );
    assert!(
        out.contains("icmp"),
        "cmp's result is compared to 0:\n{out}"
    );
}

#[test]
fn ne_is_eq_negated() {
    let src = "type M { int c; }\nbool M.eq(M o) { return c == o.c; }\nM a = M(1); M b = M(2); print(a != b);";
    let out = ir(src);
    assert!(out.contains("call M.eq"), "`!=` uses eq:\n{out}");
    assert!(out.contains("not"), "`!=` negates it:\n{out}");
}

#[test]
fn str_has_plus_and_equals_built_in() {
    let out = ir("str x = \"a\" + \"b\"; print(x == \"ab\");");
    assert!(out.contains("rt_concat"), "`+` on str concatenates:\n{out}");
    assert!(out.contains("rt_str_eq"), "`==` on str compares:\n{out}");
}

#[test]
fn a_missing_or_wrong_operator_method_is_rejected() {
    assert!(
        err("type P { int x; }\nP a = P(1); P b = P(2); print((a + b).x);")
            .contains("needs a method")
    );
    assert!(
        err("type P { int x; }\nbool P.cmp(P o) { return true; }\nP a = P(1); P b = P(2); print(a < b);")
            .contains("must return int")
    );
    assert!(
        err("type P { int x; }\nint P.add(P o) { return 1; }\nP a = P(1); P b = P(2); print((a + b).x);")
            .contains("must return P")
    );
}

// ---- interfaces -------------------------------------------------------

#[test]
fn interfaces_are_satisfied_structurally() {
    // No `implements` clause: having the methods is the proof.
    let src = "interface S { int area(); }\ntype Sq { int s; }\nint Sq.area() { return s; }\nvoid use(S x) { print(x.area()); }\nuse(Sq(2));";
    let out = ir(src);
    assert!(
        out.contains("call_iface"),
        "expected dynamic dispatch:\n{out}"
    );
}

#[test]
fn an_interface_value_is_a_plain_ref() {
    // No fat pointer: the object knows its own type through the header, so
    // `ref` stays the only reference shape in the IR.
    let src = "interface S { int area(); }\ntype Sq { int s; }\nint Sq.area() { return s; }\nvoid use(S x) { print(x.area()); }\nuse(Sq(2));";
    let out = ir(src);
    let use_fn = out
        .split("func use")
        .nth(1)
        .unwrap()
        .split("func ")
        .next()
        .unwrap();
    assert!(
        use_fn.contains("Ref"),
        "the interface parameter must be a plain ref:\n{use_fn}"
    );
}

#[test]
fn a_concrete_call_stays_static() {
    // Only interface-typed receivers dispatch; a concrete one is a direct
    // call, so interfaces cost nothing where they are not used.
    let out = ir("type Sq { int s; }\nint Sq.area() { return s; }\nSq a = Sq(2); print(a.area());");
    assert!(
        out.contains("call Sq.area"),
        "expected a direct call:\n{out}"
    );
    assert!(!out.contains("call_iface"), "no dispatch here:\n{out}");
}

#[test]
fn interface_misuse_is_rejected_with_the_reason() {
    // Naming the missing method is the difference between a diagnostic you
    // can act on and one you have to investigate.
    let e = err("interface S { int area(); }\ntype T { str t; }\nvoid use(S x) { print(x.area()); }\nuse(T(\"x\"));");
    assert!(e.contains("needs a method"), "must say why: {e}");
    assert!(e.contains("area"), "must name the method: {e}");

    assert!(err("interface S { int area(); }\nS x = S();\nprint(1);").contains("is an interface"));
    assert!(
        err("interface S { int area(); }\ntype Sq { int s; }\nint Sq.area() { return s; }\nvoid use(S x) { print(x.nope()); }\nuse(Sq(1));")
            .contains("has no method `nope`")
    );
}

// ---- embedding --------------------------------------------------------

#[test]
fn embedding_promotes_fields_and_methods() {
    let src = "type A { int v; }\nint A.get() { return v; }\ntype B { A; int w; }\nB b = B(A(1), 2); print(b.v); print(b.get());";
    let out = ir(src);
    assert!(out.contains("func B.get"), "expected a forwarder:\n{out}");
}

#[test]
fn the_outer_types_own_method_wins() {
    let src = "type A { int v; }\nint A.get() { return v; }\ntype B { A; }\nint B.get() { return 99; }\nB b = B(A(1)); print(b.get());";
    let out = ir(src);
    // Exactly one B.get, and it is the hand-written one, not a forwarder.
    assert_eq!(out.matches("func B.get").count(), 1, "{out}");
    assert!(
        out.contains("iconst 99"),
        "the hand-written body must win:\n{out}"
    );
}

#[test]
fn promotion_is_transitive() {
    // Needs a fixpoint: B.get is itself a forwarder when C embeds B, so it is
    // not visible until the round that created it has finished.
    let src = "type A { int v; }\nint A.get() { return v; }\ntype B { A; }\ntype C { B; }\nC c = C(B(A(7))); print(c.get()); print(c.v);";
    let out = ir(src);
    assert!(
        out.contains("func C.get"),
        "expected a transitive forwarder:\n{out}"
    );
}

#[test]
fn an_owned_temporary_is_released_exactly_once() {
    // Regression: producers register owned temporaries; consumers must not
    // register them again. The duplicate produced two rc_dec calls on the
    // same value, which corrupted the heap rather than failing a comparison.
    let out = ir("str f() { return \"a\" + \"b\"; }\nprint(f());");
    let main = out.split("func $main").nth(1).unwrap();
    assert_eq!(
        main.matches("rc_dec").count(),
        1,
        "exactly one release for the temporary:\n{main}"
    );
}

// ---- channels, spawn, and the move checker ----------------------------

#[test]
fn a_send_moves_and_a_recv_acquires() {
    // No retain and no release in between: the sender gives its reference up
    // and the receiver takes it. That is what keeps refcounts non-atomic.
    let out =
        ir("Chan<str> c = Chan<str>(2);\nstr s = \"a\" + \"b\";\nsend(c, s);\nprint(recv(c));");
    let main = out.split("func $main").nth(1).unwrap();
    assert_eq!(
        main.matches("rc_inc").count(),
        0,
        "a move must not retain:\n{main}"
    );
    // Two: the received string, and the channel local at scope end. The
    // channel's is a no-op, because a channel is immortal -- see
    // rt_chan_new -- but the lowering does not special-case it.
    assert_eq!(
        main.matches("rc_dec").count(),
        2,
        "the received value once, plus the channel local:\n{main}"
    );
}

#[test]
fn using_a_moved_local_is_refused() {
    assert!(
        err("Chan<str> c = Chan<str>(2);\nstr s = \"x\";\nsend(c, s);\nprint(s);")
            .contains("was moved and cannot be used again")
    );
    assert!(
        err("Chan<str> c = Chan<str>(2);\nstr s = \"x\";\nsend(c, s);\nsend(c, s);")
            .contains("was moved")
    );
    assert!(
        err("void w(str s) { print(s); }\nstr s = \"x\";\nspawn w(s);\nprint(s);")
            .contains("was moved")
    );
}

#[test]
fn an_int_needs_no_move() {
    // Only references are moved; an int is copied, so it stays usable.
    let out = ir("Chan<int> c = Chan<int>(2);\nint n = 5;\nsend(c, n);\nprint(n);");
    assert!(out.contains("rt_chan_send"), "expected a send:\n{out}");
}

#[test]
fn a_channel_is_shared_rather_than_moved() {
    // The one exemption: a channel is how threads share, so passing it to a
    // spawn aliases it. Without this the ordinary worker pattern could not
    // be written at all.
    let out = ir("void w(Chan<int> c) { send(c, 1); }\nChan<int> c = Chan<int>(2);\nspawn w(c);\nprint(recv(c));");
    assert!(out.contains("spawn w"), "expected the spawn:\n{out}");
}

#[test]
fn channel_misuse_is_rejected() {
    assert!(err("int x = 1;\nsend(x, 2);").contains("needs a channel"));
    assert!(err("int f(int a) { return a; }\nspawn f(1);").contains("needs a void function"));
    assert!(err("Chan<int> c = Chan<int>(2);\nsend(c, \"s\");").contains("expected int"));
}

// ---- distinct types ---------------------------------------------------

#[test]
fn a_distinct_type_costs_nothing_at_runtime() {
    // The whole claim: same representation as the base. No alloc, no header,
    // no refcount -- the distinctness is erased before the IR.
    let out = ir("distinct int Price;\nPrice f(Price p) { return p + p; }\nprint(f(Price(2)));");
    assert!(
        out.contains("func f(v0: I64) -> I64"),
        "must be a plain i64:\n{out}"
    );
    assert!(!out.contains("alloc"), "must not allocate:\n{out}");
    assert!(!out.contains("rc_"), "must not be refcounted:\n{out}");
}

#[test]
fn a_distinct_field_is_stored_unwrapped() {
    let out = ir(
        "distinct int UserId;\ntype User { UserId id; }\nUser u = User(UserId(7)); print(u.id);",
    );
    assert!(
        out.contains("type User { id: I64 }"),
        "the field must be a plain i64, not a reference:\n{out}"
    );
}

#[test]
fn a_distinct_type_inherits_its_bases_operations() {
    let out = ir(
        "distinct int Price;\nPrice a = Price(3); Price b = Price(4); print(a + b); print(a < b);",
    );
    assert!(out.contains("iadd"), "arithmetic is the base's:\n{out}");
    assert!(out.contains("icmp"), "comparison is the base's:\n{out}");
}

#[test]
fn a_distinct_type_does_not_mix_with_its_base_or_its_peers() {
    assert!(
        err("distinct int Price;\nPrice p = Price(1); print(p + 2);")
            .contains("cannot apply `+` to Price and int")
    );
    assert!(
        err("distinct int Price;\nPrice p = Price(1); int n = p; print(n);")
            .contains("expected int, found Price")
    );
    assert!(
        err("distinct int Price;\nPrice p = 5; print(p);").contains("expected Price, found int")
    );
    assert!(err(
        "distinct int Price;\ndistinct int UserId;\nvoid f(Price p) { print(p); }\nf(UserId(1));"
    )
    .contains("expected Price, found UserId"));
}

#[test]
fn conversions_go_both_ways_and_emit_nothing() {
    let out = ir("distinct int Price;\nPrice p = Price(5); print(int(p));");
    // One constant, one print. No conversion instruction of any kind.
    assert!(out.contains("iconst 5"), "{out}");
    assert!(!out.contains("convert") && !out.contains("cast"), "{out}");
}

// ---- collections, for-in, clone ---------------------------------------

#[test]
fn an_array_allocates_once_and_a_list_has_a_buffer() {
    let a = ir("Array<int> a = [0; 4]; print(a[0]);");
    assert!(a.contains("rt_array_new"), "{a}");
    let l = ir("List<int> l = []; l.push(1); print(l[0]);");
    assert!(l.contains("rt_list_new"), "{l}");
}

#[test]
fn indexing_a_reference_element_is_borrowed() {
    // Reading an element does not retain: the collection holds the +1, the
    // same rule as reading a field.
    let out = ir("Array<str> a = [\"\"; 2]; print(a[0]);");
    let main = out.split("func $main").nth(1).unwrap();
    // One retain for the fill argument, and no extra for the read.
    assert_eq!(
        main.matches("rc_inc").count(),
        0,
        "the read must add no refcount traffic:\n{main}"
    );
}

#[test]
fn assigning_an_element_releases_the_old_one() {
    let out = ir("Array<str> a = [\"x\"; 2]; a[0] = \"y\"; print(a[0]);");
    let main = out.split("func $main").nth(1).unwrap();
    assert!(
        main.contains("rt_index_get") && main.contains("rt_index_set"),
        "old value must be read before the store:\n{main}"
    );
    assert!(
        main.contains("rc_dec"),
        "the old element must be released:\n{main}"
    );
}

#[test]
fn for_in_increments_before_the_body() {
    // This is what makes `continue` advance the loop. An increment at the
    // bottom of the body would be skipped by it and the loop would hang.
    let out = ir("List<int> xs = []; xs.push(1); for (int x in xs) { print(x); }");
    let body = out
        .split("block2:")
        .nth(1)
        .expect("expected a loop body block");
    let add = body.find("iadd").expect("the index must be incremented");
    let get = body.find("rt_index_get").expect("the element must be read");
    assert!(
        add < get,
        "the increment must come before the element read:\n{body}"
    );
}

#[test]
fn for_in_reads_the_length_once() {
    let out = ir("List<int> xs = []; for (int x in xs) { print(x); }");
    assert_eq!(
        out.matches("rt_len_of").count(),
        1,
        "the length is read once, before the loop:\n{out}"
    );
}

#[test]
fn clone_copies_a_struct_field_by_field() {
    let out = ir("type P { int x; str s; }\nP a = P(1, \"t\"); P b = clone(a); print(b.x);");
    let main = out.split("func $main").nth(1).unwrap();
    assert!(main.contains("alloc"), "clone must allocate:\n{main}");
    assert!(
        main.matches("store").count() >= 4,
        "both fields must be copied into the new object:\n{main}"
    );
}

#[test]
fn collection_misuse_is_rejected() {
    assert!(err("Array<int> a = Array<int>(4); print(a[0]);").contains("as a literal"));
    assert!(err("Array<int> a = [0; 2]; a.push(1);").contains("needs a List"));
    assert!(err("int x = 1; print(x[0]);").contains("cannot be indexed"));
    assert!(err("List<int> xs = []; xs.push(\"no\");").contains("expected int"));
    assert!(err("int x = 1; print(clone(x));").contains("nothing to clone"));
    assert!(err("List<int> xs = []; for (str s in xs) { print(s); }")
        .contains("expected str, found int"));
}

// ---- bytes -------------------------------------------------------------

#[test]
fn bytes_is_a_type_keyword() {
    assert_eq!(
        toks("bytes b"),
        vec![Tok::KwBytes, Tok::Ident("b".into()), Tok::Eof]
    );
}

#[test]
fn a_byte_is_a_value_and_moves_no_refcount() {
    // Reading, writing and iterating a bytes touch no count: a byte is an
    // int, not a reference, so the only retain is the loop's hold on the
    // collection itself.
    let out = ir("bytes b = [1, 2]; b[0] = b[1]; for (int x in b) { print(x); }");
    let main = out.split("func $main").nth(1).unwrap();
    assert!(
        main.contains("rt_bytes_get") && main.contains("rt_bytes_set"),
        "{main}"
    );
    assert_eq!(
        main.matches("rc_inc").count(),
        1,
        "only the loop's hold on the collection:\n{main}"
    );
}

#[test]
fn a_bytes_literal_builds_through_push() {
    // Sized once for what is written, then filled: each push checks its
    // value, so a computed element traps exactly as `b.push(v)` would.
    let out = ir("int k = 3; bytes b = [1, k]; print(b.size());");
    assert!(out.contains("rt_bytes_new"), "{out}");
    assert_eq!(out.matches("rt_bytes_push").count(), 2, "{out}");
}
