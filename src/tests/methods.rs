use super::*;

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
    // `cmp` is a reserved method name, so a wrong one is refused where it is
    // DECLARED rather than at the `<` below -- the runtime calls it too, for
    // `sort`, and that call is written nowhere.
    assert!(
        err("type P { int x; }\nbool P.cmp(P o) { return true; }\nP a = P(1); P b = P(2); print(a < b);")
            .contains("must be declared `int P.cmp(P other)`")
    );
    assert!(
        err("type P { int x; }\nint P.add(P o) { return 1; }\nP a = P(1); P b = P(2); print((a + b).x);")
            .contains("must return P")
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

// ---- `this` ---------------------------------------------------------

#[test]
fn this_is_a_keyword() {
    assert_eq!(toks("this"), vec![Tok::KwThis, Tok::Eof]);
    // Only the whole word: a name that merely starts with it is a name.
    assert_eq!(
        toks("thisone"),
        vec![Tok::Ident("thisone".to_string()), Tok::Eof]
    );
}

#[test]
fn reading_this_is_borrowed_and_returning_it_retains_once() {
    // The receiver is borrowed like a parameter, so using it costs nothing;
    // a return is owned, so `return this;` must add exactly one reference
    // and release none.
    let out = ir("type C { int n = 0; }\n\
                  C C.me() { return this; }\n\
                  print(C().me().n);");
    let me = out
        .split("func ")
        .find(|f| f.starts_with("C.me"))
        .expect("C.me is emitted");
    assert_eq!(me.matches("rc_inc").count(), 1, "{me}");
    assert_eq!(me.matches("rc_dec").count(), 0, "{me}");
}

#[test]
fn the_formatter_prints_this() {
    let src = "type C { int n = 0; }\nC C.me() {\n    return this;\n}\n";
    let out = crate::reformat(src, "t").expect("formats");
    assert!(out.contains("return this;"), "{out}");
}
