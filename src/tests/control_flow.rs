use super::*;

// ---- lowering: control flow ------------------------------------------

#[test]
fn if_merge_introduces_a_block_parameter() {
    // Both arms reassign x, so the join block carries it as a parameter.
    let out = ir("int x = 1; if (x > 0) { x = 2; } else { x = 3; } print(x);");
    assert!(
        out.contains("block3(v") || out.contains("block2(v") || out.contains("block1(v"),
        "expected a join block with a parameter:\n{out}"
    );
}

#[test]
fn short_circuit_and_is_lowered_as_branches() {
    let out = ir("if (true && false) { print(1); }");
    assert!(
        out.contains("brif"),
        "&& must lower to control flow:\n{out}"
    );
}

#[test]
fn unary_minus_goes_through_checked_subtraction() {
    // Lowering -x as 0 - x is what makes -INT64_MIN trap instead of wrapping.
    let out = ir("int x = 1; print(-x);");
    assert!(
        out.contains("isub"),
        "negation must use checked subtraction:\n{out}"
    );
}

// ---- lowering: loops --------------------------------------------------

#[test]
fn while_creates_a_header_with_loop_carried_parameters() {
    // The header dominates its own body, so its parameters must exist before
    // the body is lowered. Both i and total are carried.
    let out = ir("int i = 0; int t = 0; while (i < 3) { t = t + i; i = i + 1; } print(t);");
    assert!(
        out.contains("block1(v") && out.contains("jump block1("),
        "expected a parameterised loop header with a back edge:\n{out}"
    );
    // Two carried variables means two header parameters.
    let header = out
        .lines()
        .find(|l| l.starts_with("block1("))
        .expect("no header line");
    assert_eq!(
        header.matches(',').count(),
        1,
        "expected exactly two header parameters, got: {header}"
    );
}

#[test]
fn loop_body_declarations_are_not_loop_carried() {
    // `b` is declared inside the body, so it is fresh each iteration and must
    // NOT become a header parameter.
    let out = ir("int a = 0; while (a < 3) { int b = a; a = a + b + 1; } print(a);");
    let header = out
        .lines()
        .find(|l| l.starts_with("block1("))
        .expect("no header line");
    assert!(
        !header.contains(","),
        "only `a` should be carried, got: {header}"
    );
}

#[test]
fn loop_reassigning_a_str_releases_the_previous_value() {
    // Without the release, every iteration leaks. The corpus proves this at
    // runtime via __rc_live; this proves the instruction is emitted at all.
    let out =
        ir("str s = \"\"; int i = 0; while (i < 2) { s = concat(s, \"x\"); i = i + 1; } print(s);");
    assert!(
        out.matches("rc_dec").count() >= 2,
        "expected a release inside the loop and one at scope end:\n{out}"
    );
}

#[test]
fn break_releases_locals_declared_in_the_loop_body() {
    // The break path leaves the body scope, so it must release what the body
    // allocated -- release_to_depth exists for exactly this.
    let out = ir(
        "int i = 0; while (i < 3) { str t = concat(\"a\",\"b\"); if (t.size() == 2) { break; } i = i + 1; } print(i);",
    );
    // One release on the break path, one at the normal end of the iteration.
    assert!(
        out.matches("rc_dec").count() >= 2,
        "break must release body-scope locals:\n{out}"
    );
}

#[test]
fn break_and_continue_outside_a_loop_are_rejected() {
    assert!(err("break;").contains("`break` outside a loop"));
    assert!(err("continue;").contains("`continue` outside a loop"));
}

#[test]
fn break_makes_the_exit_block_a_merge_point() {
    // Without break the exit needs no parameters. With it, the exit merges the
    // header's values with the break site's, so it must take parameters.
    let out = ir(
        "int i = 0; int f = 0; while (i < 9) { if (i > 3) { f = i; break; } i = i + 1; } print(f);",
    );
    let exit_has_params = out
        .lines()
        .filter(|l| l.starts_with("block") && l.contains("(v") && l.ends_with("):"))
        .count();
    assert!(
        exit_has_params >= 2,
        "expected both a parameterised header and a parameterised exit:\n{out}"
    );
}
