use super::*;

#[test]
fn arithmetic_precedence_matches_c() {
    // 2 + 3 * 4 must multiply first: the imul has to consume the constants,
    // and the iadd has to consume the imul's result.
    let out = ir("print(2 + 3 * 4);");
    let mul = out.find("imul").expect("expected a multiply");
    let add = out.find("iadd").expect("expected an add");
    assert!(mul < add, "multiply must be emitted before add:\n{out}");
}

#[test]
fn comparison_binds_looser_than_arithmetic() {
    let out = ir("print(1 + 2 < 4);");
    let add = out.find("iadd").expect("expected an add");
    let cmp = out.find("icmp").expect("expected a compare");
    assert!(add < cmp, "add must be emitted before compare:\n{out}");
}

#[test]
fn bit_operators_bind_tighter_than_comparison() {
    // Python's order, not C's: `x & 1 == 0` is `(x & 1) == 0`.
    let out = ir("int x = 6; print(x & 1 == 0);");
    let and = out.find("iand").expect("expected a bitwise and");
    let cmp = out.find("icmp").expect("expected a compare");
    assert!(and < cmp, "and must be emitted before compare:\n{out}");
}

#[test]
fn shifts_bind_looser_than_addition() {
    let out = ir("int x = 1; print(x << x + 1);");
    let add = out.find("iadd").expect("expected an add");
    let shl = out.find("ishl").expect("expected a shift");
    assert!(add < shl, "add must be emitted before shift:\n{out}");
}

#[test]
fn right_shift_needs_adjacent_angle_brackets() {
    assert!(err("int x = 8; print(x > > 1);").contains("expected an expression"));
}
