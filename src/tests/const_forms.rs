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

// ---- user-type module constants -------------------------------------------

fn c(src: &str) -> String {
    compile_str(src, "c").expect("expected this program to compile")
}

#[test]
fn a_user_type_constant_is_static_data() {
    let out = c("type P { int x; float w; bool b; str s; }\n\
                 type L { P head; List<P> rest; }\n\
                 const L LINE = L(P(1, 0.5, true, \"a\"), [P(2, -0.0, false, \"b\")]);\n\
                 print(LINE.head.x);\n");
    // Three structs and a list, all static and immortal, the parts first.
    assert!(out.contains("static const T"), "{out}");
    assert!(out.contains("RC_IMMORTAL, &ti_T"), "{out}");
    assert!(out.contains("5.0000000000000000e-1"), "{out}");
    assert!(out.contains("-0.0000000000000000e0"), "{out}");
    assert!(out.contains("(Obj *)&str"), "{out}");
    assert!(out.contains("(Obj *)&k"), "{out}");
    // Nothing is built at run time.
    let ir = ir("type P { int x; }\nconst P O = P(3);\nprint(O.x);\n");
    let main = func_ir(&ir, "$main");
    assert!(!main.contains("alloc"), "{main}");
    assert!(main.contains("kconst"), "{main}");
}

#[test]
fn a_user_type_constant_takes_defaults_from_its_declaration() {
    ir("const int BASE = 4;\n\
        type P { int x; int y = BASE * 2; List<int> zs = [BASE]; }\n\
        const P O = P(1);\nconst P Q = P(1, y: 3);\nprint(O.y + Q.y + O.zs[0]);\n");
    let e = err("int f() { return 1; }\ntype P { int x; int y = f(); }\nconst P O = P(1);\n");
    assert!(e.contains("not a constant expression"), "{e}");
}

#[test]
fn a_user_type_constant_never_changes() {
    let e = err("type P { int x; }\ntype H { List<P> ps; }\n\
                 const H C = H([P(1)]);\nC.ps[0].x = 2;\n");
    assert!(e.contains("`C` is const and cannot be changed"), "{e}");
    let e = err("type P { int x; }\nconst P C = P(1);\nC = P(2);\n");
    assert!(e.contains("cannot assign to const `C`"), "{e}");
}

#[test]
fn a_user_type_constant_refuses_what_cannot_be_static() {
    let e = err(
        "type R { int fd; }\nvoid R.drop() { }\ntype H { List<R> rs; }\n\
                 const H C = H([]);\n",
    );
    assert!(e.contains("a constant cannot hold `R`"), "{e}");
    assert!(e.contains("constant holds no I/O"), "{e}");
    let e = err("enum E { A; B(int); }\ntype H { E e; }\nconst H C = H(E.A);\n");
    assert!(
        e.contains("`E` is not; it is the type of field `e` of `H`"),
        "{e}"
    );
    let e = err("interface G { int of(); }\ntype H { G g; }\nconst H C = H(() => 1);\n");
    assert!(e.contains("`G` is not"), "{e}");
    let e = err("type P { int x; }\nint f() { return 1; }\nconst P C = P(f());\n");
    assert!(e.contains("not a constant expression"), "{e}");
    let e = err("type P { int x; }\ntype Q { int x; }\nconst P C = Q(1);\n");
    assert!(e.contains("type mismatch"), "{e}");
}

#[test]
fn a_type_that_reaches_itself_can_be_a_constant() {
    ir("type N { int v; List<N> kids; }\n\
        const N T = N(1, [N(2, []), N(3, [N(4, [])])]);\n\
        print(T.kids[1].kids[0].v);\n");
}

// ---- const types -----------------------------------------------------------

const MONEY: &str = "type P { int x; }\n\
                     const type M { int c; P p; List<P> ps; }\n\
                     void f(P p, M m) { print(p.x + m.c); }\n";

#[test]
fn a_const_type_is_frozen_at_its_construction() {
    let out = ir(&format!(
        "{MONEY}void g(P p) {{ M m = M(1, p, []); print(m.c); }}\n"
    ));
    let g = func_ir(&out, "g");
    let alloc = g.find("alloc").expect("constructed");
    let snap = g.find("call rt_snapshot(").expect("frozen at construction");
    assert!(alloc < snap, "{g}");
    // A type that is not `const` is not.
    let out = ir("type Q { int c; }\nvoid g() { Q q = Q(1); print(q.c); }\n");
    assert!(!out.contains("rt_snapshot"), "{out}");
}

#[test]
fn nothing_in_a_const_type_changes() {
    for (body, what) in [
        ("void g(M m) { m.c = 2; }", "a field"),
        ("void g(M m) { m.p.x = 2; }", "a field of a field"),
        ("void g(M m) { m.ps.push(P(1)); }", "a collection field"),
        ("void g(M m) { m.ps[0] = P(1); }", "an element"),
        ("void M.g() { c = 2; }", "a bare field in a method"),
        (
            "void M.g() { ps.clear(); }",
            "a bare collection field in a method",
        ),
        (
            "type H { M m; }\nvoid g(H h) { h.m.c = 2; }",
            "through a mutable holder",
        ),
    ] {
        let e = err(&format!("{MONEY}{body}\n"));
        assert!(e.contains("`M` is a `const type`"), "{what}: {e}");
    }
    // The holder of one, and a collection of them, are ordinary.
    ir(&format!(
        "{MONEY}type H {{ M m; }}\nvoid g(H h, List<M> ms) {{ h.m = M(1, P(1), []); ms[0] = h.m; }}\n"
    ));
}

#[test]
fn a_chain_out_of_a_const_type_is_not_held() {
    // Every instance is frozen for good, so `m.p` cannot be overwritten
    // while `f` runs, and nor can `this.p` in one of its methods.
    let out = ir(&format!(
        "{MONEY}void g(M m) {{ f(m.p, m); f(m.ps[0], m); }}\n\
         void M.go() {{ f(p, this); f(ps[0], this); }}\n"
    ));
    for name in ["g", "M.go"] {
        let body = func_ir(&out, name);
        assert_eq!(body.matches("rc_inc").count(), 0, "{body}");
    }
}

#[test]
fn a_const_type_passes_a_const_parameter() {
    ir(&format!(
        "{MONEY}int k(const M m) {{ return m.c; }}\nint px(const P p) {{ return p.x; }}\n\
         M make() {{ return M(1, P(2), []); }}\n\
         void g(M m) {{ print(k(m) + k(make()) + px(m.p)); }}\n\
         void M.go() {{ print(px(p)); }}\n"
    ));
}

#[test]
fn clone_of_a_const_type_is_the_value_itself() {
    let out = ir(&format!("{MONEY}M g(M m) {{ return clone(m); }}\n"));
    let g = func_ir(&out, "g");
    assert!(!g.contains("clone") && !g.contains("alloc"), "{g}");
}

#[test]
fn a_const_type_holds_no_resource() {
    let e = err("type R { int fd; }\nvoid R.drop() { }\nconst type M { List<R> rs; }\n");
    assert!(
        e.contains("`M` is a `const type`, and it can hold `R`"),
        "{e}"
    );
    let e = err("const type M { int c; }\nvoid M.drop() { }\n");
    assert!(e.contains("it has a destructor"), "{e}");
}

#[test]
fn only_a_struct_can_be_const() {
    for decl in ["const interface I { int f(); }", "const enum E { A; }"] {
        let e = err(&format!("{decl}\n"));
        assert!(e.contains("only a `type` can be `const`"), "{e}");
    }
}
