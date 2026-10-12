use super::*;

// ---- destructors ----------------------------------------------------

#[test]
fn a_destructor_alone_earns_a_drop_function_but_no_walk() {
    // A type with only an int field needs no release and no walk -- but a
    // destructor still has to be called from somewhere, so it gets a drop
    // function. The walk function only reports references, and there are
    // none, so it stays NULL.
    let c = compile_str(
        "type G { int n; }\nvoid G.drop() { print(n); }\nG g = G(1);",
        "c",
    )
    .expect("compiles");
    // Which T<i> is G: the drop function that calls G's destructor.
    let i = c
        .split("static void drop_T")
        .find(|b| b.contains("G___drop(o)"))
        .and_then(|b| b.split('(').next())
        .unwrap_or_else(|| panic!("no drop function calls G.drop:\n{c}"));
    let ti = c
        .lines()
        .find(|l| l.contains(&format!("TypeInfo ti_T{i} ")))
        .unwrap_or_else(|| panic!("no TypeInfo for T{i}:\n{c}"));
    assert!(
        ti.ends_with(&format!(
            "{{ drop_T{i}, NULL, NULL, copy_T{i}, \"G\", NULL, NULL, NULL }};"
        )),
        "drop set, walk NULL, copy for a const snapshot, the name the runtime \
         reports when a const meets a value owning a resource, and no `cmp`, \
         `hash` or `eq`: {ti}"
    );
    assert!(
        c.contains("o->rc = 1;"),
        "destructor must run at count 1:\n{c}"
    );
}

#[test]
fn a_destructor_runs_before_the_fields_are_released() {
    let c = compile_str(
        "type N { str s; }\nvoid N.drop() { print(s); }\nN x = N(\"a\" + \"b\");",
        "c",
    )
    .expect("compiles");
    let body = c
        .split("static void drop_T")
        .find(|b| b.contains("N___drop(o)"))
        .unwrap_or_else(|| panic!("no drop function calls N.drop:\n{c}"));
    let call = body.find("N___drop(o)").unwrap();
    let release = body.find("rc_dec(p->f_s)").expect("the field is released");
    assert!(call < release, "destructor must come first:\n{body}");
}

#[test]
fn a_destructor_is_not_promoted_by_embedding() {
    let out = ir("type A { int n; }\nvoid A.drop() { print(n); }\n\
                  type B { A; }\nB b = B(A(1));");
    assert!(out.contains("func A.drop"), "{out}");
    assert!(!out.contains("func B.drop"), "{out}");
}

#[test]
fn a_destructor_on_an_unused_generic_type_is_still_checked() {
    let e = err("type Wrap<T> { T v; }\nint Wrap<T>.drop() { return 1; }\nprint(1);");
    assert!(e.contains("must return `void`"), "{e}");
}

#[test]
fn the_reserved_methods_go_into_the_typeinfo_uncast() {
    // `cmp`, `hash` and `eq` are how the RUNTIME orders and hashes a user
    // type, so they are fields of the TypeInfo beside the destructor rather
    // than vtable slots. No `(AnyFn)` on any of them: the emitted
    // definitions already have the prototypes rt.h declares, so a mismatch
    // is a C compile error rather than a wrong call at run time.
    let c = compile_str(
        "type K { int v; }\n\
         int K.cmp(K o) { return v - o.v; }\n\
         bool K.eq(K o) { return v == o.v; }\n\
         int K.hash() { return v; }\n\
         K k = K(1);",
        "c",
    )
    .expect("compiles");
    let ti = c
        .lines()
        .find(|l| l.contains("TypeInfo ti_T") && l.contains("K___cmp"))
        .unwrap_or_else(|| panic!("no TypeInfo carries K.cmp:\n{c}"));
    assert!(
        ti.ends_with("fn_K___cmp, fn_K___hash, fn_K___eq };"),
        "cmp, hash and eq, in that order and with no cast: {ti}"
    );
}

#[test]
fn a_type_without_them_leaves_the_typeinfo_slots_null() {
    let c = compile_str("type K { int v; }\nK k = K(1);", "c").expect("compiles");
    let ti = c
        .lines()
        .find(|l| l.contains("TypeInfo ti_T"))
        .unwrap_or_else(|| panic!("no TypeInfo:\n{c}"));
    assert!(ti.ends_with("NULL, NULL, NULL };"), "{ti}");
}

#[test]
fn a_cmp_promoted_by_embedding_is_not_the_outer_types_cmp() {
    // The forwarder is `int Outer.cmp(Inner)`, which the runtime would call
    // with two Outers. Both have the IR shape `int64 (Obj *, Obj *)`, so
    // only a check on the WRITTEN signature catches it.
    let e = err("type Inner { int v; }\n\
                 int Inner.cmp(Inner o) { return v - o.v; }\n\
                 type Outer { Inner; }\n\
                 List<Outer> xs = [Outer(Inner(1))];\n\
                 xs.sort();");
    assert!(
        e.contains("inherits `cmp` from the embedded `Inner`"),
        "{e}"
    );
}

#[test]
fn a_reserved_name_with_another_shape_is_refused_where_it_is_declared() {
    // Not at a use: `sort` and a map's probe call these from the runtime, so
    // there is no call site in the program to hang the message on.
    let e = err("type K { int v; }\nstr K.hash() { return str(v); }\nprint(1);");
    assert!(e.contains("must be declared `int K.hash()`"), "{e}");
    let e = err("type K { int v; }\nint K.hash(int salt) { return v; }\nprint(1);");
    assert!(e.contains("this one takes a parameter"), "{e}");
    // An interface parameter is the one other honest reading of `cmp`, and
    // is allowed -- it is just not a `cmp` `sort` will take.
    let ok = ir("interface Ord { int cmp(Ord other); }\n\
                 type C { int n; }\n\
                 int C.cmp(Ord other) { return n; }\n\
                 Ord o = C(1);\nprint(o.cmp(o));");
    assert!(ok.contains("call_iface"), "{ok}");
}

// ---- moving a `case` binding ----------------------------------------

#[test]
fn a_case_binding_crosses_a_thread_by_being_taken_out_of_its_enum() {
    // Not a retain: the payload slot is CLEARED, so the enum's release skips
    // it and the receiver holds the only reference. The uniqueness check runs
    // on the ENUM, whose graph contains the payload.
    let out = ir(
        "Result<List<int>, str> parse() { return Result<List<int>, str>.Ok([1]); }\n\
                  void run(Chan<List<int>> ch) {\n\
                  match (parse()) {\n\
                  case Ok(List<int> xs): { send(ch, xs); }\n\
                  case Err(str e): { print(e); }\n\
                  }\n\
                  }\n\
                  print(1);",
    );
    assert!(out.contains("take T"), "{out}");
}

#[test]
fn a_resource_that_cannot_be_cloned_is_not_told_to_clone_itself() {
    let e = err("type Conn { int fd; }\n\
                 void Conn.drop() { print(fd); }\n\
                 void hand(Conn c, Chan<Conn> out) { send(out, c); }\n\
                 print(1);");
    assert!(e.contains("cannot be cloned"), "{e}");
    assert!(!e.contains("clone(c)"), "{e}");
    // A type without a destructor still gets the old, correct advice.
    let e = err("type Box { int n; }\n\
                 void hand(Box b, Chan<Box> out) { send(out, b); }\n\
                 print(1);");
    assert!(e.contains("Use clone(b) to send a copy."), "{e}");
}
