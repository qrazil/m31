use super::*;

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
