use super::*;

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

/// The IR of function `name` alone, out of a whole module's.
fn func_ir(out: &str, name: &str) -> String {
    out.split("func ")
        .find(|f| f.starts_with(&format!("{name}(")))
        .unwrap_or_else(|| panic!("`{name}` is not emitted:\n{out}"))
        .to_string()
}

/// Declarations shared by the hold tests below: a callee that can replace
/// the field its first argument was read from.
const HOLD_DECLS: &str = "type P { int x; }\n\
                          type H { P p; str s; List<int> ps; }\n\
                          void f(P p, H h) { h.p = P(0); print(p.x); }\n\
                          int k(H h) { h.ps = []; return 1; }\n";

#[test]
fn a_field_argument_is_held_across_the_call() {
    // docs/ir-v0.md §5.1: the caller keeps a borrowed argument alive. `h.p`
    // is kept alive only by `h`, which `f` can overwrite, so the caller
    // retains it before the call and releases it after.
    let out = ir(&format!("{HOLD_DECLS}void g(H h) {{ f(h.p, h); }}\n"));
    let g = func_ir(&out, "g");
    assert_eq!(g.matches("rc_inc").count(), 1, "{g}");
    assert_eq!(g.matches("rc_dec").count(), 1, "{g}");
    let (inc, call, dec) = (
        g.find("rc_inc").unwrap(),
        g.find("call f(").unwrap(),
        g.find("rc_dec").unwrap(),
    );
    assert!(inc < call && call < dec, "retain, call, release:\n{g}");
}

#[test]
fn a_local_or_parameter_argument_is_not_held() {
    // A frame already holds a local or a parameter, and no expression can
    // reassign one, so passing it stays free -- the reason arguments are
    // borrowed in the first place (§5.3).
    let out = ir(&format!(
        "{HOLD_DECLS}void g(P p, H h) {{ f(p, h); P q = P(1); f(q, h); }}\n"
    ));
    let g = func_ir(&out, "g");
    // The only traffic is `q`'s own release at the end of its scope.
    assert_eq!(g.matches("rc_inc").count(), 0, "{g}");
    assert_eq!(g.matches("rc_dec").count(), 1, "{g}");
}

#[test]
fn a_field_is_held_from_the_moment_it_is_read() {
    // Left to right: in `two(h.p, k(h))` the second argument runs before the
    // call, so the retain must come before it, not merely before `two`.
    let out = ir(&format!(
        "{HOLD_DECLS}int two(P p, int n) {{ return p.x + n; }}\n\
         int g(H h) {{ return two(h.p, k(h)); }}\n"
    ));
    let g = func_ir(&out, "g");
    let inc = g.find("rc_inc").expect("h.p must be held");
    let later = g.find("call k(").unwrap();
    assert!(inc < later, "held before the later argument runs:\n{g}");
}

#[test]
fn a_place_read_that_nothing_can_disturb_is_not_held() {
    // Only the runtime runs between these reads and their last use, and the
    // runtime writes no field or element a program can see: no hold.
    let out = ir(&format!(
        "{HOLD_DECLS}int g(H h, int i) {{\n\
             h.ps.push(i + 1);\n\
             print(h.s);\n\
             print(concat(h.s, \"!\"));\n\
             h.p.x = h.ps[i - 1];\n\
             return h.ps[h.ps.size() - 1] + h.s.size();\n\
         }}\n"
    ));
    let g = func_ir(&out, "g");
    assert_eq!(g.matches("rc_inc").count(), 0, "{g}");
}

#[test]
fn a_receiver_is_held_when_an_argument_runs_code() {
    // `h.ps.push(k(h))`: `k` replaces `h.ps` before `push` writes into it.
    let out = ir(&format!("{HOLD_DECLS}void g(H h) {{ h.ps.push(k(h)); }}\n"));
    let g = func_ir(&out, "g");
    assert_eq!(g.matches("rc_inc").count(), 1, "{g}");
    let inc = g.find("rc_inc").unwrap();
    assert!(inc < g.find("call k(").unwrap(), "{g}");
}

#[test]
fn a_left_operand_is_held_only_when_the_right_one_runs_code() {
    let pure = ir(&format!(
        "{HOLD_DECLS}str g(H h) {{ return h.s + \"x\"; }}\n"
    ));
    let g = func_ir(&pure, "g");
    assert_eq!(g.matches("rc_inc").count(), 0, "{g}");

    let code = ir(&format!(
        "{HOLD_DECLS}str s(H h) {{ h.s = \"z\"; return \"t\"; }}\n\
         str g(H h) {{ return h.s + s(h); }}\n"
    ));
    let g = func_ir(&code, "g");
    assert_eq!(g.matches("rc_inc").count(), 1, "{g}");
}

#[test]
fn a_releasing_receiver_is_held_only_when_a_destructor_exists() {
    // `clear` releases every element; only a destructor can make that run
    // user code, so only a program with one pays for the hold.
    let src = "type D { int n; }\ntype H { List<D> ds; }\n\
               void g(H h) { h.ds.clear(); }\n";
    let without = ir(src);
    assert_eq!(func_ir(&without, "g").matches("rc_inc").count(), 0);

    let with = ir(&format!("{src}void D.drop() {{ print(n); }}\n"));
    let g = func_ir(&with, "g");
    assert_eq!(g.matches("rc_inc").count(), 1, "{g}");
}

#[test]
fn a_field_of_a_const_local_is_not_held() {
    // The object a `const` local names is frozen for as long as the name is
    // in scope, so nothing -- `f` included -- can overwrite `c.p`: the store
    // traps first. The only traffic is the local's own +1 and its release.
    let out = ir(&format!(
        "{HOLD_DECLS}void g(H h) {{ const H c = h; f(c.p, c); }}\n"
    ));
    let g = func_ir(&out, "g");
    let call = g.find("call f(").unwrap();
    assert!(!g[..call].contains("rc_inc v2"), "c.p is not held:\n{g}");
    assert_eq!(g.matches("rc_inc").count(), 1, "{g}");
    assert_eq!(g.matches("rc_dec").count(), 1, "{g}");
}

#[test]
fn a_field_of_a_value_frozen_by_a_const_block_is_held_only_outside_it() {
    // Inside `const h { .. }` the object is frozen until the block ends,
    // which cannot happen in the middle of the call: no hold. The same
    // expression after the block is an ordinary place read, and is held.
    let out = ir(&format!(
        "{HOLD_DECLS}void g(H h) {{ const h {{ f(h.p, h); }} f(h.p, h); }}\n"
    ));
    let g = func_ir(&out, "g");
    let leave = g.find("call rt_freeze_leave(").unwrap();
    let (inside, after) = g.split_at(leave);
    assert!(inside.contains("call rt_freeze_enter("), "{g}");
    assert!(inside.contains("call f("), "{g}");
    assert!(
        !inside.contains("rc_inc") && !inside.contains("rc_dec"),
        "{g}"
    );
    assert_eq!(after.matches("rc_inc").count(), 1, "{g}");
    assert_eq!(after.matches("rc_dec").count(), 1, "{g}");
}

#[test]
fn a_field_of_a_mutable_local_is_still_held() {
    // `m` names the same object as the parameter, but nothing froze it.
    let out = ir(&format!(
        "{HOLD_DECLS}void g(H h) {{ H m = h; f(m.p, m); }}\n"
    ));
    let g = func_ir(&out, "g");
    let call = g.find("call f(").unwrap();
    // `m`'s own retain, then the hold on `m.p`; two releases after.
    assert_eq!(g[..call].matches("rc_inc").count(), 2, "{g}");
    assert_eq!(g[call..].matches("rc_dec").count(), 2, "{g}");
}

#[test]
fn only_the_first_step_out_of_a_frozen_root_is_unheld() {
    // A `const` block freezes the named object alone: `o.h` is not frozen,
    // so `o.h.p` can still be overwritten while `o` cannot, and is held.
    // An element of a frozen list is not; a promoted field, read through
    // the embedded object, is held like any field of an unfrozen object.
    let out = ir(&format!(
        "{HOLD_DECLS}type O {{ H h; }}\n\
         type E {{ P q; }}\n\
         type W {{ E; }}\n\
         void j(P p, O o) {{ o.h.p = P(1); print(p.x); }}\n\
         void E.reset() {{ q = P(1); }}\n\
         void m(P p, W w) {{ w.reset(); print(p.x); }}\n\
         void deep(O o) {{ const o {{ j(o.h.p, o); }} }}\n\
         void elem(List<P> xs, H h) {{ const xs {{ f(xs[0], h); }} }}\n\
         void promoted(W w) {{ const w {{ m(w.q, w); }} }}\n"
    ));
    let deep = func_ir(&out, "deep");
    assert_eq!(deep.matches("rc_inc").count(), 1, "{deep}");
    let elem = func_ir(&out, "elem");
    assert_eq!(elem.matches("rc_inc").count(), 0, "{elem}");
    let promoted = func_ir(&out, "promoted");
    assert_eq!(promoted.matches("rc_inc").count(), 1, "{promoted}");
}

#[test]
fn a_field_read_through_the_receiver_is_still_held() {
    // Whether `this` is frozen is the caller's knowledge: nothing at the
    // call site inside the method can show it, so a bare field is held.
    let out = ir(&format!(
        "{HOLD_DECLS}void H.go() {{ f(p, this); }}\n\
         void g(H h) {{ const h {{ h.go(); }} }}\n"
    ));
    let go = func_ir(&out, "H.go");
    assert_eq!(go.matches("rc_inc").count(), 1, "{go}");
}

#[test]
fn an_element_of_a_module_constant_is_not_held() {
    // A constant's graph is static and immortal, at every depth.
    let out = ir(&format!(
        "{HOLD_DECLS}const List<List<str>> T = [[\"a\"], [\"b\"]];\n\
         void say(str s, H h) {{ h.s = s; print(s); }}\n\
         void g(H h) {{ say(T[1][0], h); }}\n"
    ));
    let g = func_ir(&out, "g");
    assert_eq!(g.matches("rc_inc").count(), 0, "{g}");
    assert_eq!(g.matches("rc_dec").count(), 0, "{g}");
}
