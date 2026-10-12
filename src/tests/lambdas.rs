use super::*;

#[test]
fn a_lambda_is_two_tokens_at_the_arrow() {
    // `=>` is one token and has exactly one use, which is what lets the
    // parser decide a lambda by looking at what follows the closing
    // parenthesis instead of at what is inside it.
    assert_eq!(
        toks("=> = == >="),
        vec![Tok::FatArrow, Tok::Assign, Tok::EqEq, Tok::GtEq, Tok::Eof]
    );
}

#[test]
fn a_lambda_with_no_captures_never_allocates() {
    // A lambda that mentions nothing from its scope has no captures, so its
    // synthesised type has no fields, so Stage 2's rule applies to it: one
    // immortal static instance, constructed by taking its address. This is
    // the claim the corpus cannot see, and the reason the feature is free
    // for the commonest callback of all.
    let c = compile_str(
        "interface Get { int of(); }\n\
         int use(Get g) { return g.of(); }\n\
         print(use(() => 3));",
        "c",
    )
    .expect("compiles");
    // Which T<i> is the lambda: the one whose vtable holds its method.
    let vt = c
        .lines()
        .find(|l| l.contains("static const AnyFn vt_T") && l.contains("__lam"))
        .unwrap_or_else(|| panic!("no vtable for a lambda type:\n{c}"));
    let i = vt
        .split("vt_T")
        .nth(1)
        .and_then(|s| s.split('[').next())
        .expect("a type number");
    assert!(
        c.contains(&format!("static T{i} imm_T{i} ")),
        "the lambda's type must have one static instance:\n{c}"
    );
    assert!(
        c.contains(&format!("= &imm_T{i}.hdr;")),
        "the lambda must be that instance, not an allocation:\n{c}"
    );
    // One `rt_alloc` of this type exists in the file and it is the generated
    // `copy_T`, which every type gets and which this one never reaches: a
    // snapshot hands a frozen object straight back, and RC_IMMORTAL is all
    // bits set. Nothing on any path the program runs allocates it.
    assert_eq!(
        c.matches(&format!("rt_alloc(sizeof(T{i})")).count(),
        1,
        "a captureless lambda must never allocate outside copy_T{i}:\n{c}"
    );
    let copy = c
        .split(&format!("static Obj *copy_T{i}"))
        .nth(1)
        .and_then(|b| b.split("\n}").next())
        .unwrap_or_else(|| panic!("no copy function for T{i}:\n{c}"));
    assert!(
        copy.contains(&format!("rt_alloc(sizeof(T{i})")),
        "the one allocation must be the generated copy:\n{copy}"
    );
}

#[test]
fn a_lambdas_captures_are_the_types_fields() {
    // The whole design rests on this: a capture is a field store, so every
    // rule about a field -- the retain, the release, the deep copy a `const`
    // takes, the destructor -- reaches captures with no new code. A `str`
    // capture is a reference, so the type gets a walk and a release for it;
    // an `int` capture is copied into a slot and gets neither.
    let c = compile_str(
        "interface Get { int of(); }\n\
         int use(Get g) { return g.of(); }\n\
         str s = \"a\" + \"b\";\n\
         int n = 1;\n\
         print(use(() => s.size() + n));",
        "c",
    )
    .expect("compiles");
    let ty = c
        .split("typedef struct")
        .find(|b| b.contains("f_s") && b.contains("f_n"))
        .unwrap_or_else(|| panic!("no type holds both captures:\n{c}"));
    assert!(
        ty.contains("Obj *f_s;") && ty.contains("int64_t f_n;"),
        "a reference capture is a slot, an int capture is a word: {ty}"
    );
    // And it allocates, because it has fields: this is the contrast with
    // the test above, not an exception to it.
    // `} T7;` closes the struct and names it.
    let i = ty
        .split("} T")
        .nth(1)
        .and_then(|s| s.split(';').next())
        .expect("a type number");
    assert!(
        c.contains(&format!("rt_alloc(sizeof(T{i})")),
        "a capturing lambda is an ordinary construction:\n{c}"
    );
}

#[test]
fn a_lambda_is_not_a_source_of_inference() {
    // Information flows from the target interface to the lambda, never back.
    // With no target there is no method name, no arity and no return type to
    // check against, so the lambda is refused rather than guessed at.
    let e = err("print((int x) => x);");
    assert!(e.contains("nothing here expects one"), "{e}");
    let e = err("int y = (int a) => a;");
    assert!(e.contains("`int` is not one"), "{e}");
}
