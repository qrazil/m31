use super::*;

#[test]
fn generics_are_erased_before_lowering() {
    // The IR must contain no type parameters at all -- only instantiations.
    let out = ir("type Wrap<T> { T value; }\nWrap<int> b = Wrap<int>(1); print(b.value);");
    assert!(
        out.contains("type Wrap$int"),
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
        "type Wrap<T> { T value; }\nWrap<int> a = Wrap<int>(1); Wrap<int> b = Wrap<int>(2); Wrap<str> c = Wrap<str>(\"s\"); print(a.value + b.value); print(c.value);",
    );
    assert_eq!(
        out.matches("type Wrap$int").count(),
        1,
        "Wrap<int> must be instantiated exactly once:\n{out}"
    );
    assert!(out.contains("type Wrap$str"), "Wrap<str> missing:\n{out}");
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
        "type Wrap<T> { T value; }\nT unwrap<T>(Wrap<T> b) { return b.value; }\nWrap<int> b = Wrap<int>(1); print(unwrap(b));",
    );
    assert!(
        out.contains("func unwrap$int"),
        "expected the inferred instantiation:\n{out}"
    );
}

#[test]
fn generic_misuse_is_rejected() {
    assert!(
        err("type Wrap<T> { T value; }\nWrap<int,str> b = Wrap<int,str>(1); print(b.value);")
            .contains("takes 1 type argument(s), found 2")
    );
    assert!(err("type P { int x; }\nP<int> p = P<int>(1); print(p.x);").contains("is not generic"));
    // Inference cannot see through a nested call; the diagnostic says what to
    // do rather than guessing.
    assert!(err(
        "type Wrap<T> { T value; }\nT unwrap<T>(Wrap<T> b) { return b.value; }\nWrap<Wrap<int>> n = Wrap<Wrap<int>>(Wrap<int>(1)); print(unwrap(unwrap(n)));"
    )
    .contains("bind an argument, or the result, to a local"));
}
