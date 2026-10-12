use super::*;

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
