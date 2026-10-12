use super::*;

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
