use super::*;

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
