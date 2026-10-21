//! The three forms of `const` (docs/const-decision.md, "The third
//! decision"): a `const` parameter, a user-type module constant and a
//! `const` type. The corpus proves the programs behave; these prove what is
//! refused, and what the frozen roots save in the IR.

use super::*;

fn func_ir(out: &str, name: &str) -> String {
    out.split("func ")
        .find(|f| f.starts_with(&format!("{name}(")))
        .unwrap_or_else(|| panic!("`{name}` is not emitted:\n{out}"))
        .to_string()
}

const DECLS: &str = "type P { int x; }\n\
                     type H { P p; List<P> ps; }\n\
                     int n(const List<int> xs) { return xs.size(); }\n\
                     int px(const P p) { return p.x; }\n\
                     const List<int> TABLE = [1, 2];\n";

// ---- const parameters ------------------------------------------------------

#[test]
fn a_const_parameter_accepts_every_kind_of_frozen_argument() {
    ir(&format!(
        "{DECLS}int fwd(const List<int> xs) {{ return n(xs); }}\n\
         int deep(const H h) {{ return px(h.p) + px(h.ps[0]); }}\n\
         void g(List<int> b, H h) {{\n\
             const List<int> c = [1];\n\
             print(n(c) + n(TABLE) + fwd(TABLE));\n\
             const b {{ print(n(b)); }}\n\
             const h {{ print(px(h.p) + px(h.ps[0]) + deep(h)); }}\n\
         }}\n"
    ));
}

#[test]
fn a_const_parameter_refuses_what_is_not_frozen() {
    for arg in ["b", "[1]", "clone(TABLE)"] {
        let e = err(&format!(
            "{DECLS}void g(List<int> b) {{ print(n({arg})); }}\n"
        ));
        assert!(e.contains("`xs` is a `const` parameter"), "{arg}: {e}");
    }
    // A mutable local is told how to freeze it for the call.
    let e = err(&format!("{DECLS}void g(List<int> b) {{ print(n(b)); }}\n"));
    assert!(e.contains("`const b { .. }`"), "{e}");
    // A field of a mutable value, and a bare field through the receiver.
    let e = err(&format!("{DECLS}void g(H h) {{ print(px(h.p)); }}\n"));
    assert!(e.contains("`p` is a `const` parameter"), "{e}");
    let e = err(&format!("{DECLS}int H.go() {{ return px(p); }}\n"));
    assert!(e.contains("`p` is a `const` parameter"), "{e}");
    // After the block, the same name is mutable again.
    let e = err(&format!(
        "{DECLS}void g(List<int> b) {{ const b {{ print(n(b)); }} print(n(b)); }}\n"
    ));
    assert!(e.contains("`xs` is a `const` parameter"), "{e}");
}

#[test]
fn a_scalar_or_str_const_parameter_takes_any_argument() {
    ir("int f(const int x, const str s) { return x + s.size(); }\n\
        int y = 2; str t = \"ab\"; print(f(y, t));");
}

#[test]
fn nothing_changes_through_a_const_parameter() {
    let e = err(&format!(
        "{DECLS}void g(const List<int> xs) {{ xs.push(1); }}\n"
    ));
    assert!(e.contains("`xs` is const and cannot be changed"), "{e}");
    let e = err(&format!("{DECLS}void g(const H h) {{ h.ps[0].x = 1; }}\n"));
    assert!(e.contains("`h` is const and cannot be changed"), "{e}");
    let e = err(&format!(
        "{DECLS}void g(const H h) {{ h = H(P(1), []); }}\n"
    ));
    assert!(e.contains("cannot assign to const `h`"), "{e}");
}

#[test]
fn a_chain_out_of_a_const_parameter_is_not_held() {
    // The argument was frozen at the call site and stays frozen for the
    // whole call, so nothing can overwrite `h.p` or `h.ps[0]` while `f`
    // runs: no retain, no release, at any depth.
    let out = ir("type P { int x; }\n\
                  type H { P p; List<P> ps; }\n\
                  void f(P p, H h) { print(p.x + h.ps.size()); }\n\
                  void g(const H h) { f(h.p, h); f(h.ps[0], h); }\n\
                  void m(H h) { f(h.p, h); }\n");
    let g = func_ir(&out, "g");
    assert_eq!(g.matches("rc_inc").count(), 0, "{g}");
    assert_eq!(g.matches("rc_dec").count(), 0, "{g}");
    // The same read out of a parameter that is not `const` is held.
    let m = func_ir(&out, "m");
    assert_eq!(m.matches("rc_inc").count(), 1, "{m}");
}

#[test]
fn a_const_parameter_costs_nothing_at_run_time() {
    let out = ir(&format!(
        "{DECLS}void g() {{ const List<int> c = [1]; print(n(c)); }}\n"
    ));
    let callee = func_ir(&out, "n");
    assert!(
        !callee.contains("rt_freeze")
            && !callee.contains("rt_snapshot")
            && !callee.contains("rt_check"),
        "{callee}"
    );
    let g = func_ir(&out, "g");
    assert_eq!(g.matches("rt_snapshot").count(), 1, "only the local's: {g}");
    assert!(!g.contains("rt_freeze"), "{g}");
}

#[test]
fn const_is_part_of_a_signature_that_must_match() {
    let iface = "interface Sum { int of(const List<int> xs); }\n";
    // A method that does not say `const` does not satisfy a slot that does,
    // and the other way round.
    let e = err(&format!(
        "{iface}type T {{ int k; }}\nint T.of(List<int> xs) {{ return 1; }}\nSum s = T(1);\n"
    ));
    assert!(e.contains("with a matching signature"), "{e}");
    let e = err("interface Sum { int of(List<int> xs); }\n\
                 int g(const List<int> xs) { return 1; }\nSum s = g;\n");
    assert!(e.contains("same parameter types and `const`"), "{e}");
    let e = err("interface Sum { int of(List<int> xs); }\n\
                 Sum s = (const List<int> xs) => xs.size();\n");
    assert!(e.contains("this lambda takes (const List<int>)"), "{e}");
    // A call through the interface is held to the check the body relies on.
    let e = err(&format!(
        "{iface}int g(Sum s, List<int> b) {{ return s.of(b); }}\n"
    ));
    assert!(e.contains("`xs` is a `const` parameter"), "{e}");
    // Matching exactly compiles, for a type, a function and a lambda.
    ir(&format!(
        "{iface}type T {{ int k; }}\nint T.of(const List<int> xs) {{ return xs.size(); }}\n\
         int g(const List<int> xs) {{ return xs.size(); }}\n\
         const List<int> C = [1];\n\
         Sum a = T(1); Sum b = g; Sum c = (const List<int> xs) => xs.size();\n\
         print(a.of(C) + b.of(C) + c.of(C));\n"
    ));
}

#[test]
fn a_const_parameters_default_must_be_a_constant() {
    let e = err("void f(const List<int> xs = [1]) { }\nf();\n");
    assert!(e.contains("`xs` is a `const` parameter"), "{e}");
    ir("const List<int> D = [1];\nint f(const List<int> xs = D) { return xs.size(); }\nprint(f());\n");
}

#[test]
fn a_spawned_const_parameter_needs_a_value_frozen_for_good() {
    let w = "void w(const List<int> xs) { print(xs.size()); }\nconst List<int> T = [1];\n";
    ir(&format!(
        "{w}void go() {{ const List<int> c = [1]; spawn w(c); spawn w(T); }}\n"
    ));
    let e = err(&format!(
        "{w}void go(List<int> b) {{ const b {{ spawn w(b); }} }}\n"
    ));
    assert!(e.contains("must be frozen for good"), "{e}");
    let e = err(&format!(
        "{w}void go(const List<int> b) {{ spawn w(b); }}\n"
    ));
    assert!(e.contains("must be frozen for good"), "{e}");
}

#[test]
fn a_const_parameter_cannot_hold_a_resource() {
    let e = err("type R { int fd; }\nvoid R.drop() { }\nvoid f(const R r) { }\n");
    assert!(e.contains("`const` cannot hold `R`"), "{e}");
}

#[test]
fn a_field_cannot_be_const() {
    let e = err("type P { const int x; }\n");
    assert!(e.contains("a field cannot be `const`"), "{e}");
}
