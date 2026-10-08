//! Unit tests.
//!
//! These complement the corpus rather than duplicating it. The corpus proves
//! that compiled programs BEHAVE correctly, end to end, against four C
//! builds and a Go twin. These prove the compiler's internals: that the lexer
//! rejects what it should, and that lowering produces the IR we think it
//! does.
//!
//! The IR snapshots matter because the corpus cannot see them. A lowering bug
//! that happens to produce correct output -- a redundant retain/release pair,
//! a block that is never reached, an extra temporary -- is invisible at the
//! C level and obvious here.

use crate::{compile_str, lexer::Lexer, lexer::Tok};

fn ir(src: &str) -> String {
    compile_str(src, "ir").expect("expected this program to compile")
}

fn err(src: &str) -> String {
    match compile_str(src, "c") {
        Ok(_) => panic!("expected this program to be rejected"),
        Err(d) => d.to_string(),
    }
}

fn toks(src: &str) -> Vec<Tok> {
    Lexer::new(src)
        .tokenize()
        .expect("expected this to lex")
        .into_iter()
        .map(|t| t.tok)
        .collect()
}

// ---- lexer ----------------------------------------------------------

#[test]
fn lexes_two_character_operators_greedily() {
    assert_eq!(
        toks("== = != ! <= < >= > && ||"),
        vec![
            Tok::EqEq,
            Tok::Assign,
            Tok::BangEq,
            Tok::Bang,
            Tok::LtEq,
            Tok::Lt,
            Tok::GtEq,
            Tok::Gt,
            Tok::AmpAmp,
            Tok::PipePipe,
            Tok::Eof,
        ]
    );
}

#[test]
fn bit_operators_lex_and_right_shift_stays_two_tokens() {
    // `>>` is never one token: the same two characters close
    // `List<List<int>>`, so the parser joins them, and only in operator
    // position.
    assert_eq!(
        toks("& && | || ^ ~ << <= >> >="),
        vec![
            Tok::Amp,
            Tok::AmpAmp,
            Tok::Pipe,
            Tok::PipePipe,
            Tok::Caret,
            Tok::Tilde,
            Tok::Shl,
            Tok::LtEq,
            Tok::Gt,
            Tok::Gt,
            Tok::GtEq,
            Tok::Eof,
        ]
    );
}

#[test]
fn underscores_in_int_literals_are_separators() {
    assert_eq!(toks("1_000_000"), vec![Tok::Int(1_000_000), Tok::Eof]);
}

#[test]
fn prefixed_int_literals_lex_to_their_bits() {
    let one = |src: &str| match toks(src).as_slice() {
        [Tok::Int(n), Tok::Eof] => *n,
        t => panic!("{src} lexed to {t:?}"),
    };
    assert_eq!(one("0x1F"), 31);
    assert_eq!(one("0xff"), one("0xFF"));
    assert_eq!(one("0o17"), 15);
    assert_eq!(one("0b101"), 5);
    assert_eq!(one("0b1_0__1_"), 5, "the decimal separator rule");
    assert_eq!(one("0"), 0);
    // A prefixed literal is 64 bits, the top one the sign.
    assert_eq!(one("0x7FFF_FFFF_FFFF_FFFF"), i64::MAX);
    assert_eq!(one("0x8000_0000_0000_0000"), i64::MIN);
    assert_eq!(one("0xFFFF_FFFF_FFFF_FFFF"), -1);
    assert_eq!(one(&format!("0b{}", "1".repeat(64))), -1);
    // A method on a prefixed literal is still a method call.
    assert_eq!(
        toks("0x10.to_str"),
        vec![
            Tok::Int(16),
            Tok::Dot,
            Tok::Ident("to_str".into()),
            Tok::Eof
        ]
    );
}

#[test]
fn malformed_prefixed_int_literals_are_refused() {
    let msg = |src: &str| match Lexer::new(src).tokenize() {
        Ok(t) => panic!("{src} lexed to {t:?}"),
        Err(d) => d.to_string(),
    };
    for (src, want) in [
        ("017", "write `17`, or `0o17` for octal"),
        ("0_17", "write `17`, or `0o17` for octal"),
        ("00", "write `0`, or `0o0` for octal"),
        ("09", "write `9`"),
        ("0X1F", "write `0x`, not `0X`"),
        ("0O17", "write `0o`, not `0O`"),
        ("0B1", "write `0b`, not `0B`"),
        ("0x", "`0x` must be followed by a hex digit"),
        ("0x_1", "`0x` must be followed by a hex digit"),
        ("0o8", "`8` is not an octal digit"),
        ("0b102", "`2` is not a binary digit"),
        ("0x1G", "`G` is not a hex digit"),
        ("0x1.5", "a float literal is written in decimal"),
        ("0x1_0000_0000_0000_0000", "does not fit in 64 bits"),
        (&format!("0b1{}", "0".repeat(64)), "does not fit in 64 bits"),
        // A decimal literal is a number, not bits: it still has to fit.
        ("18446744073709551615", "does not fit in int"),
    ] {
        let got = msg(src);
        assert!(got.contains(want), "{src}: {got}");
    }
    // `09` has no octal reading to offer.
    assert!(!msg("09").contains("0o"));
    // A leading zero is refused in an integer only: `0.5` is a float.
    assert_eq!(toks("0.5"), vec![Tok::Float(0.5), Tok::Eof]);
}

#[test]
fn formatting_keeps_every_int_spelling() {
    let src =
        "int a = 0x1F;\nint b = 0o755 | 0b1010_0101;\nint c = 1_000_000;\nint d = 0xdead_BEEF;\n";
    let once = crate::reformat(src, "t").expect("formats");
    assert_eq!(once, src);
}

#[test]
fn string_literals_keep_multibyte_utf8_intact() {
    // Regression: accumulating `byte as char` decoded each UTF-8
    // continuation byte as its own Latin-1 codepoint and mangled the string.
    assert_eq!(
        toks(r#""héllo →""#),
        vec![Tok::Str("héllo →".into()), Tok::Eof]
    );
}

#[test]
fn a_character_literal_is_the_code_point_as_an_int() {
    // No new token and no new type: `'*'` lexes to the int 42, exactly as
    // `42` does, and `'é'` to the scalar value rather than to a UTF-8 byte.
    assert_eq!(toks("'*'"), vec![Tok::Int(42), Tok::Eof]);
    assert_eq!(toks("'0'"), vec![Tok::Int(48), Tok::Eof]);
    assert_eq!(toks("'é'"), vec![Tok::Int(233), Tok::Eof]);
    assert_eq!(toks("'中'"), vec![Tok::Int(20013), Tok::Eof]);
    assert_eq!(toks(r"'\n'"), vec![Tok::Int(10), Tok::Eof]);
    assert_eq!(toks(r"'\0'"), vec![Tok::Int(0), Tok::Eof]);
    assert_eq!(toks(r"'\\'"), vec![Tok::Int(92), Tok::Eof]);
    assert_eq!(toks(r"'\''"), vec![Tok::Int(39), Tok::Eof]);
    assert_eq!(toks(r"'\u{1F600}'"), vec![Tok::Int(0x1F600), Tok::Eof]);
    // `"` needs no escape inside `'`, and `'` needs none inside `"`.
    assert_eq!(toks("'\"'"), vec![Tok::Int(34), Tok::Eof]);
    assert_eq!(toks("\"it's\""), vec![Tok::Str("it's".into()), Tok::Eof]);
}

#[test]
fn a_character_literal_is_exactly_one_character() {
    // Each literal escapes its own quote and no other, so there is one
    // spelling for each -- which is what lets the formatter keep the
    // author's.
    for (src, want) in [
        ("int c = '';", "empty character literal"),
        ("int c = 'ab';", "exactly one character"),
        ("int c = '\\\"';", "unknown escape"),
        ("str s = \"\\'\";", "unknown escape"),
        ("int c = '\\xff';", "`\\x` stops at 7f"),
        ("int c = 'a;", "unterminated character literal"),
    ] {
        let got = err(src);
        assert!(got.contains(want), "{src}: {got}");
    }
}

#[test]
fn formatting_keeps_every_char_spelling() {
    // A character literal is an int, so printing the value would put back
    // the magic number the literal exists to remove.
    let src = "int a = '*';\nint b = '\\n';\nint c = 'é';\nint d = '\\u{e9}';\n";
    let once = crate::reformat(src, "t").expect("formats");
    assert_eq!(once, src);
}

#[test]
fn the_range_for_advances_before_the_body() {
    // The increment must be at the TOP of the body, or `continue` would
    // skip it and the loop would not terminate. That is the whole reason
    // the construct exists, so it is checked in the IR and not only by a
    // program that would hang if it were wrong.
    let out = ir("for (int i in 0 .. 3) {\n    continue;\n}\n");
    let body = out
        .split("brif")
        .nth(1)
        .expect("expected a loop header with a conditional branch");
    let add = body.find("iadd").expect("expected the increment");
    let jump = body.find("jump").expect("expected the back edge");
    assert!(
        add < jump,
        "the increment must precede the back edge:\n{out}"
    );
}

#[test]
fn a_range_is_loop_syntax_and_the_counter_is_the_loops() {
    for (src, want) in [
        ("int a = 0 .. 3;", "may only be written in a `for` header"),
        ("print((0 .. 3));", "may only be written in a `for` header"),
        (
            "for (str s in 0 .. 3) { print(s); }",
            "a range counts in `int`",
        ),
        (
            "for (int i in \"a\" .. 3) { print(i); }",
            "expected int, found str",
        ),
        (
            "for (int i in 0 .. 3) { i = 9; }",
            "is the loop's variable and cannot be assigned",
        ),
        (
            "List<int> xs = [1]; for (int v in xs) { v = 9; }",
            "is the loop's variable and cannot be assigned",
        ),
    ] {
        let got = err(src);
        assert!(got.contains(want), "{src}: {got}");
    }
}

#[test]
fn the_absent_loop_and_assignment_forms_are_named() {
    // §9 exists so that an absence is a decision. A decision should reach
    // the person who trips over it, and these are the two things everyone
    // writes in their first hour here.
    for (src, want) in [
        (
            "for (int i = 0; i < 3; i = i + 1) { print(i); }",
            "there is no three-clause `for`",
        ),
        ("int n = 1;\nn += 1;", "there is no `+=`"),
        ("int n = 1;\nn -= 1;", "there is no `-=`"),
        ("int n = 1;\nn *= 2;", "there is no `*=`"),
        ("int n = 1;\nn /= 2;", "there is no `/=`"),
        ("int n = 1;\nn %= 2;", "there is no `%=`"),
        ("int n = 1;\nn &= 2;", "there is no `&=`"),
        ("int n = 1;\nn |= 2;", "there is no `|=`"),
        ("int n = 1;\nn ^= 2;", "there is no `^=`"),
        ("int n = 1;\nn <<= 2;", "there is no `<<=`"),
        ("int n = 1;\nn >>= 2;", "there is no `>>=`"),
    ] {
        let got = err(src);
        assert!(got.contains(want), "{src}: {got}");
    }
}

#[test]
fn the_formatter_keeps_the_parentheses_the_author_wrote() {
    // They do not survive into the AST, so the formatter is told where they
    // were -- the same bargain as a literal's spelling.
    let src = concat!(
        "int a = 1;\nint b = 2;\nint c = 3;\nint d = 4;\n",
        "print((a > 0 && b > 0) || (c > 0 && d > 0));\n",
        "print(d ^ (b & (c ^ d)));\n",
        "print((a & b) != 0);\n",
        "print((a));\n",
        "print(a + b * c);\n",
    );
    let once = crate::reformat(src, "t").expect("formats");
    assert_eq!(once, src);
    // And a group around a group is one group, so it is still a fixed point.
    let twice = crate::reformat("print(((a)));\n", "t").expect("formats");
    assert_eq!(twice, "print((a));\n");
    assert_eq!(
        crate::reformat(&twice, "t").expect("formats again"),
        twice,
        "collapsing `((a))` must reach a fixed point"
    );
}

#[test]
fn a_case_binds_every_value_or_none() {
    let decl = "enum E { A; B(int, str); }\n";
    // None: the spelling a payload-less variant already uses.
    let out = ir(&format!(
        "{decl}int f(E e) {{\n    match (e) {{\n        case A: {{ return 0; }}\n        \
         case B: {{ return 1; }}\n    }}\n}}\nprint(f(E.B(1, \"x\")));\n"
    ));
    // The arm reads the tag and nothing else: no payload is projected.
    assert!(out.contains(" = tag "), "{out}");
    assert!(!out.contains("payload"), "{out}");
    // Some of them is still refused, and the message offers the other rule.
    let got = err(&format!(
        "{decl}int f(E e) {{\n    match (e) {{\n        case A: {{ return 0; }}\n        \
         case B(int n): {{ return n; }}\n    }}\n}}\nprint(f(E.A));\n"
    ));
    assert!(got.contains("write `case B:` to bind none"), "{got}");
    // And it is NOT a `default`: every variant still needs its own case.
    let got = err(&format!(
        "{decl}int f(E e) {{\n    match (e) {{\n        case A: {{ return 0; }}\n    }}\n}}\n\
         print(f(E.A));\n"
    ));
    assert!(got.contains("missing B"), "{got}");
}

#[test]
fn a_case_payload_may_be_discarded_with_an_underscore() {
    let decl = "enum E { A; B(int, str); }\n";
    // Some of the payloads, by position: the first is projected, the second
    // is not.
    let out = ir(&format!(
        "{decl}int f(E e) {{\n    match (e) {{\n        case A: {{ return 0; }}\n        \
         case B(int n, _): {{ return n; }}\n    }}\n}}\nprint(f(E.B(1, \"x\")));\n"
    ));
    assert_eq!(out.matches("payload").count(), 1, "{out}");
    // None of them: no payload is projected at all.
    let out = ir(&format!(
        "{decl}int f(E e) {{\n    match (e) {{\n        case A: {{ return 0; }}\n        \
         case B(_, _): {{ return 1; }}\n    }}\n}}\nprint(f(E.A));\n"
    ));
    assert!(!out.contains("payload"), "{out}");
    // It still stands for a position, so the count is checked.
    let got = err(&format!(
        "{decl}int f(E e) {{\n    match (e) {{\n        case A: {{ return 0; }}\n        \
         case B(_): {{ return 1; }}\n    }}\n}}\nprint(f(E.A));\n"
    ));
    assert!(
        got.contains("carries 2 value(s), and this case binds 1"),
        "{got}"
    );
    // It is not a value, a name, or something with a type.
    let got = err("print(_);\n");
    assert!(
        got.contains("`_` is a discard and cannot be used as a value"),
        "{got}"
    );
    let got = err("int _ = 1;\n");
    assert!(got.contains("`_` is a discard, not a name"), "{got}");
    let got = err(&format!(
        "{decl}int f(E e) {{\n    match (e) {{\n        case A: {{ return 0; }}\n        \
         case B(int _, _): {{ return 1; }}\n    }}\n}}\nprint(f(E.A));\n"
    ));
    assert!(got.contains("a discard is written `_` alone"), "{got}");
    // `_foo` is an ordinary identifier; only the lone underscore is special.
    assert_eq!(
        toks("_foo _"),
        vec![Tok::Ident("_foo".into()), Tok::Underscore, Tok::Eof]
    );
}

#[test]
fn string_escapes_are_decoded() {
    assert_eq!(
        toks(r#""a\tb\nc\\d\"e""#),
        vec![Tok::Str("a\tb\nc\\d\"e".into()), Tok::Eof]
    );
}

#[test]
fn lexer_rejects_bad_input() {
    let bad = |src: &str| Lexer::new(src).tokenize().is_err();
    assert!(bad(r#""unterminated"#), "unterminated string");
    assert!(bad("\"has a\nnewline\""), "newline inside string");
    assert!(bad(r#""\q""#), "unknown escape");
    assert!(bad("/* unterminated"), "unterminated block comment");
    assert!(bad("123abc"), "suffix on integer");
    assert!(
        bad("9223372036854775808"),
        "integer literal too large for int"
    );
    assert!(bad("a @ b"), "unexpected character");
}

#[test]
fn comments_are_trivia() {
    assert_eq!(
        toks("1 // line\n/* block */ 2"),
        vec![Tok::Int(1), Tok::Int(2), Tok::Eof]
    );
}

#[test]
fn hex_and_unicode_escapes_decode_to_utf8_bytes() {
    let one = |src: &str| match toks(src).as_slice() {
        [Tok::Str(s), Tok::Eof] => s.clone(),
        other => panic!("expected one string literal, got {other:?}"),
    };
    assert_eq!(one(r#""\x41\x7f\x00\x0A""#), "A\x7f\0\n");
    assert_eq!(one(r#""\x4a\x4A""#), "JJ");
    assert_eq!(one(r#""\r""#), "\r");
    assert_eq!(one(r#""\u{e9}""#), "é");
    assert_eq!(one(r#""\u{00E9}9""#), "é9");
    assert_eq!(one(r#""\u{0}""#), "\0");
    assert_eq!(one(r#""\u{1F600}""#), "\u{1F600}");
    assert_eq!(one(r#""\u{10FFFF}""#), "\u{10FFFF}");
    // Raw control characters and a raw BOM are bytes like any other.
    assert_eq!(one("\"\x01\u{feff}\""), "\x01\u{feff}");
}

#[test]
fn bad_escapes_are_refused() {
    let bad = |src: &str| Lexer::new(src).tokenize().is_err();
    assert!(bad(r#""\x80""#), "\\x past 7f: not UTF-8 on its own");
    assert!(bad(r#""\xff""#), "\\x past 7f");
    assert!(bad(r#""\x4""#), "\\x with one digit");
    assert!(bad(r#""\xg1""#), "\\x with a non-hex digit");
    assert!(bad(r#""\u41""#), "\\u without braces");
    assert!(bad(r#""\u{}""#), "braces with no digits");
    assert!(bad(r#""\u{1234567}""#), "seven digits");
    assert!(bad(r#""\u{41""#), "no closing brace");
    assert!(bad(r#""\u{d800}""#), "a surrogate");
    assert!(bad(r#""\u{110000}""#), "past U+10FFFF");
    assert!(bad(r#""\a""#), "C's \\a is not an escape here");
    assert!(bad("\"\\"), "backslash at end of file");
    assert!(bad("\"\\\n\""), "backslash at end of line");
}

/// Every character the round-trip tests put in a literal: all of U+0000 to
/// U+00FF (each byte value, as the character a lone byte cannot be in UTF-8),
/// and a spread of the kinds `{:?}` used to escape -- combining marks,
/// format characters, bidi controls, private use, noncharacters, the plane
/// edges.
fn awkward_chars() -> Vec<char> {
    let mut cs: Vec<char> = (0u32..=0xff).filter_map(char::from_u32).collect();
    for c in [
        0x300, 0x301, 0x36f, 0x85, 0x200b, 0x200d, 0x200e, 0x2028, 0x2029, 0x202e, 0x2066, 0xd7ff,
        0xe000, 0xf8ff, 0xfdd0, 0xfeff, 0xfffd, 0xfffe, 0xffff, 0x10000, 0x1f600, 0xe0001, 0xf0000,
        0x10fffd, 0x10ffff,
    ] {
        cs.push(char::from_u32(c).unwrap());
    }
    cs
}

#[test]
fn quote_lexes_back_to_the_same_string() {
    let cs = awkward_chars();
    let mut all = String::new();
    for &c in &cs {
        all.push(c);
        for s in [c.to_string(), format!("a{c}b"), format!("{c}{c}")] {
            let q = crate::lexer::quote(&s);
            assert_eq!(
                toks(&q),
                vec![Tok::Str(s.clone()), Tok::Eof],
                "quote({s:?}) = {q}"
            );
        }
    }
    assert_eq!(
        toks(&crate::lexer::quote(&all)),
        vec![Tok::Str(all), Tok::Eof]
    );
}

/// The string literals a source lexes to, in order.
fn str_lits(src: &str) -> Vec<String> {
    toks(src)
        .into_iter()
        .filter_map(|t| match t {
            Tok::Str(s) => Some(s),
            _ => None,
        })
        .collect()
}

#[test]
fn formatting_keeps_every_literal_byte_for_byte() {
    // Each character three ways: raw where a raw one can stand, as `\u{}`,
    // and as `\x` where it is ASCII. `m31c fmt` must hand back a file that
    // lexes to the same strings -- it used to write `\u{1}`, Rust's escape,
    // which this lexer refuses.
    let mut src = String::new();
    for c in awkward_chars() {
        let n = c as u32;
        if !matches!(c, '\n' | '"' | '\\') {
            src.push_str(&format!("print(\"<{c}>\");\n"));
        }
        src.push_str(&format!("print(\"<\\u{{{n:x}}}>\");\n"));
        if n < 0x80 {
            src.push_str(&format!("print(\"<\\x{n:02x}>\");\n"));
        }
    }
    let once = crate::reformat(&src, "t").expect("formats");
    assert_eq!(str_lits(&once), str_lits(&src));
    // And it keeps the spelling, so formatting is the identity here.
    assert_eq!(once, src);
    let q = crate::reformat(&once, "t").expect("formats again");
    assert_eq!(q, once);
}

// ---- parser ---------------------------------------------------------

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

// ---- lowering: refcounting -------------------------------------------

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

// ---- diagnostics ------------------------------------------------------

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

// ---- generics ---------------------------------------------------------

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

// ---- arguments --------------------------------------------------------

#[test]
fn mandatory_is_positional_and_optional_is_named() {
    let out = ir("int f(int a, int b = 7) { return a + b; }\nprint(f(1));\nprint(f(1, b: 2));");
    assert!(out.contains("func f"), "expected the function:\n{out}");
}

#[test]
fn defaults_are_filled_in_at_the_call_site() {
    // The default is an expression evaluated at the call, so the constant
    // appears in the caller rather than the callee.
    let out = ir("int f(int a, int b = 7) { return a + b; }\nprint(f(1));");
    let caller = out.split("func $main").nth(1).unwrap();
    assert!(
        caller.contains("iconst 7"),
        "the default must be materialised by the caller:\n{caller}"
    );
}

#[test]
fn the_argument_rule_is_enforced() {
    // Naming a mandatory parameter is the mistake someone arriving from
    // Python makes, and it gets the specific message rather than an arity one.
    assert!(err("int f(int a) { return a; }\nprint(f(a: 1));")
        .contains("`a` is mandatory, so it is positional"));
    assert!(
        err("int f(int a, int b = 1) { return a; }\nprint(f(1, 2));")
            .contains("takes 1 positional argument(s), found 2")
    );
    assert!(
        err("int f(int a, int b = 1) { return a; }\nprint(f(1, c: 2));")
            .contains("has no parameter `c`")
    );
    assert!(
        err("int f(int a, int b = 1) { return a; }\nprint(f(b: 2, 1));")
            .contains("positional arguments must come before named ones")
    );
    assert!(
        err("int f(int a, int b = 1) { return a; }\nprint(f(1, b: 2, b: 3));")
            .contains("given twice")
    );
}

#[test]
fn construction_uses_the_same_rule() {
    let out = ir("type P { int x; str tag = \"none\"; }\nP p = P(1);\nprint(p.tag);");
    assert!(out.contains(" = alloc "), "expected construction:\n{out}");
    assert!(err("type P { int x; }\nP p = P(x: 1);\nprint(p.x);")
        .contains("`x` is mandatory, so it is positional"));
}

#[test]
fn type_names_need_no_capital() {
    // Recognition is by declaration, not by spelling: the parser collects
    // every `type IDENT` in a pre-pass. Capitalisation is a convention the
    // language does not enforce.
    let out = ir("type point { int x; }\npoint p = point(1);\nprint(p.x);");
    assert!(out.contains("type point"), "expected the type:\n{out}");
}

#[test]
fn a_function_may_not_share_a_name_with_a_type() {
    // A type wins in construction position, so the function would be
    // silently unreachable -- and with no capitalisation rule, the collision
    // is easy to hit by accident.
    assert!(
        err("type foo { int x; }\nint foo(int n) { return n; }\nprint(1);")
            .contains("`foo` is already a type")
    );
}

// ---- methods and shadowing --------------------------------------------

#[test]
fn methods_are_declared_by_qualified_name() {
    let out = ir("type R { int w; }\nint R.area() { return w; }\nR r = R(2); print(r.area());");
    assert!(out.contains("func R.area"), "expected the method:\n{out}");
}

#[test]
fn a_bare_name_in_a_method_reads_the_field() {
    // No `this`: the field is reached bare, and borrowed from the receiver.
    let out = ir(
        "type R { int w; int h; }\nint R.area() { return w * h; }\nR r = R(2,3); print(r.area());",
    );
    let m = out.split("func $main").next().unwrap();
    assert_eq!(
        m.matches("load").count(),
        2,
        "both fields must be read from the receiver:\n{m}"
    );
}

#[test]
fn a_method_may_assign_a_field_by_bare_name() {
    let out =
        ir("type R { int w; }\nvoid R.grow() { w = w + 1; }\nR r = R(1); r.grow(); print(r.w);");
    let m = out.split("func $main").next().unwrap();
    assert!(m.contains("store"), "expected a field store:\n{m}");
}

#[test]
fn the_receiver_is_borrowed_like_any_argument() {
    let out = ir("type R { int w; }\nint R.get() { return w; }\nR r = R(1); print(r.get());");
    let m = out.split("func $main").next().unwrap();
    assert!(
        !m.contains("rc_inc") && !m.contains("rc_dec"),
        "a method must not touch its receiver's refcount:\n{m}"
    );
}

#[test]
fn nothing_shadows_anything() {
    let msg = "shadowing is not allowed";
    // an outer local
    assert!(err("int x = 1; if (true) { int x = 2; print(x); } print(x);").contains(msg));
    // a parameter
    assert!(err("int f(int a) { int a = 2; return a; }\nprint(f(1));").contains(msg));
    // a loop's enclosing local
    assert!(err("int i = 0; while (i < 3) { int i = 9; print(i); }").contains(msg));
    // a function
    assert!(err("int helper() { return 1; }\nint helper = 5; print(helper);").contains(msg));
    // a field of the receiver
    assert!(
        err("type R { int w; }\nint R.bad() { int w = 5; return w; }\nprint(1);").contains(msg)
    );
}

#[test]
fn sibling_scopes_may_reuse_a_name() {
    // Not shadowing: neither is visible to the other.
    let out = ir(
        "int t = 0; if (true) { int s = 1; t = s; } if (t > 0) { int s = 2; t = t + s; } print(t);",
    );
    assert!(out.contains("func $main"), "expected it to compile:\n{out}");
}

// ---- operator overloading ---------------------------------------------

#[test]
fn operators_desugar_to_methods() {
    let src = "type M { int c; }\nM M.add(M o) { return M(c + o.c); }\nM a = M(1); M b = M(2); print((a + b).c);";
    let out = ir(src);
    assert!(
        out.contains("call M.add"),
        "`+` must call the method:\n{out}"
    );
}

#[test]
fn comparison_goes_through_a_single_cmp() {
    // One implementation gives a total order; four separate methods could be
    // made inconsistent with each other.
    let src = "type M { int c; }\nint M.cmp(M o) { return c - o.c; }\nM a = M(1); M b = M(2); print(a < b); print(a >= b);";
    let out = ir(src);
    assert_eq!(
        out.matches("call M.cmp").count(),
        2,
        "both comparisons go through cmp:\n{out}"
    );
    assert!(
        out.contains("icmp"),
        "cmp's result is compared to 0:\n{out}"
    );
}

#[test]
fn ne_is_eq_negated() {
    let src = "type M { int c; }\nbool M.eq(M o) { return c == o.c; }\nM a = M(1); M b = M(2); print(a != b);";
    let out = ir(src);
    assert!(out.contains("call M.eq"), "`!=` uses eq:\n{out}");
    assert!(out.contains("not"), "`!=` negates it:\n{out}");
}

#[test]
fn str_has_plus_and_equals_built_in() {
    let out = ir("str x = \"a\" + \"b\"; print(x == \"ab\");");
    assert!(out.contains("rt_concat"), "`+` on str concatenates:\n{out}");
    assert!(out.contains("rt_str_eq"), "`==` on str compares:\n{out}");
}

#[test]
fn a_missing_or_wrong_operator_method_is_rejected() {
    assert!(
        err("type P { int x; }\nP a = P(1); P b = P(2); print((a + b).x);")
            .contains("needs a method")
    );
    // `cmp` is a reserved method name, so a wrong one is refused where it is
    // DECLARED rather than at the `<` below -- the runtime calls it too, for
    // `sort`, and that call is written nowhere.
    assert!(
        err("type P { int x; }\nbool P.cmp(P o) { return true; }\nP a = P(1); P b = P(2); print(a < b);")
            .contains("must be declared `int P.cmp(P other)`")
    );
    assert!(
        err("type P { int x; }\nint P.add(P o) { return 1; }\nP a = P(1); P b = P(2); print((a + b).x);")
            .contains("must return P")
    );
}

// ---- interfaces -------------------------------------------------------

#[test]
fn interfaces_are_satisfied_structurally() {
    // No `implements` clause: having the methods is the proof.
    let src = "interface S { int area(); }\ntype Sq { int s; }\nint Sq.area() { return s; }\nvoid use(S x) { print(x.area()); }\nuse(Sq(2));";
    let out = ir(src);
    assert!(
        out.contains("call_iface"),
        "expected dynamic dispatch:\n{out}"
    );
}

#[test]
fn an_interface_value_is_a_plain_ref() {
    // No fat pointer: the object knows its own type through the header, so
    // `ref` stays the only reference shape in the IR.
    let src = "interface S { int area(); }\ntype Sq { int s; }\nint Sq.area() { return s; }\nvoid use(S x) { print(x.area()); }\nuse(Sq(2));";
    let out = ir(src);
    let use_fn = out
        .split("func use")
        .nth(1)
        .unwrap()
        .split("func ")
        .next()
        .unwrap();
    assert!(
        use_fn.contains("Ref"),
        "the interface parameter must be a plain ref:\n{use_fn}"
    );
}

#[test]
fn a_concrete_call_stays_static() {
    // Only interface-typed receivers dispatch; a concrete one is a direct
    // call, so interfaces cost nothing where they are not used.
    let out = ir("type Sq { int s; }\nint Sq.area() { return s; }\nSq a = Sq(2); print(a.area());");
    assert!(
        out.contains("call Sq.area"),
        "expected a direct call:\n{out}"
    );
    assert!(!out.contains("call_iface"), "no dispatch here:\n{out}");
}

#[test]
fn interface_misuse_is_rejected_with_the_reason() {
    // Naming the missing method is the difference between a diagnostic you
    // can act on and one you have to investigate.
    let e = err("interface S { int area(); }\ntype T { str t; }\nvoid use(S x) { print(x.area()); }\nuse(T(\"x\"));");
    assert!(e.contains("needs a method"), "must say why: {e}");
    assert!(e.contains("area"), "must name the method: {e}");

    assert!(err("interface S { int area(); }\nS x = S();\nprint(1);").contains("is an interface"));
    assert!(
        err("interface S { int area(); }\ntype Sq { int s; }\nint Sq.area() { return s; }\nvoid use(S x) { print(x.nope()); }\nuse(Sq(1));")
            .contains("has no method `nope`")
    );
}

// ---- embedding --------------------------------------------------------

#[test]
fn embedding_promotes_fields_and_methods() {
    let src = "type A { int v; }\nint A.get() { return v; }\ntype B { A; int w; }\nB b = B(A(1), 2); print(b.v); print(b.get());";
    let out = ir(src);
    assert!(out.contains("func B.get"), "expected a forwarder:\n{out}");
}

#[test]
fn the_outer_types_own_method_wins() {
    let src = "type A { int v; }\nint A.get() { return v; }\ntype B { A; }\nint B.get() { return 99; }\nB b = B(A(1)); print(b.get());";
    let out = ir(src);
    // Exactly one B.get, and it is the hand-written one, not a forwarder.
    assert_eq!(out.matches("func B.get").count(), 1, "{out}");
    assert!(
        out.contains("iconst 99"),
        "the hand-written body must win:\n{out}"
    );
}

#[test]
fn promotion_is_transitive() {
    // Needs a fixpoint: B.get is itself a forwarder when C embeds B, so it is
    // not visible until the round that created it has finished.
    let src = "type A { int v; }\nint A.get() { return v; }\ntype B { A; }\ntype C { B; }\nC c = C(B(A(7))); print(c.get()); print(c.v);";
    let out = ir(src);
    assert!(
        out.contains("func C.get"),
        "expected a transitive forwarder:\n{out}"
    );
}

#[test]
fn an_owned_temporary_is_released_exactly_once() {
    // Regression: producers register owned temporaries; consumers must not
    // register them again. The duplicate produced two rc_dec calls on the
    // same value, which corrupted the heap rather than failing a comparison.
    let out = ir("str f() { return \"a\" + \"b\"; }\nprint(f());");
    let main = out.split("func $main").nth(1).unwrap();
    assert_eq!(
        main.matches("rc_dec").count(),
        1,
        "exactly one release for the temporary:\n{main}"
    );
}

// ---- channels, spawn, and the move checker ----------------------------

#[test]
fn a_send_moves_and_a_recv_acquires() {
    // No retain and no release in between: the sender gives its reference up
    // and the receiver takes it. That is what keeps refcounts non-atomic.
    let out =
        ir("Chan<str> c = Chan<str>(2);\nstr s = \"a\" + \"b\";\nsend(c, s);\nprint(recv(c));");
    let main = out.split("func $main").nth(1).unwrap();
    assert_eq!(
        main.matches("rc_inc").count(),
        0,
        "a move must not retain:\n{main}"
    );
    // Two: the received string, and the channel local at scope end. The
    // channel's is a no-op, because a channel is immortal -- see
    // rt_chan_new -- but the lowering does not special-case it.
    assert_eq!(
        main.matches("rc_dec").count(),
        2,
        "the received value once, plus the channel local:\n{main}"
    );
}

#[test]
fn using_a_moved_local_is_refused() {
    assert!(
        err("Chan<str> c = Chan<str>(2);\nstr s = \"x\";\nsend(c, s);\nprint(s);")
            .contains("was moved and cannot be used again")
    );
    assert!(
        err("Chan<str> c = Chan<str>(2);\nstr s = \"x\";\nsend(c, s);\nsend(c, s);")
            .contains("was moved")
    );
    assert!(
        err("void w(str s) { print(s); }\nstr s = \"x\";\nspawn w(s);\nprint(s);")
            .contains("was moved")
    );
}

#[test]
fn an_int_needs_no_move() {
    // Only references are moved; an int is copied, so it stays usable.
    let out = ir("Chan<int> c = Chan<int>(2);\nint n = 5;\nsend(c, n);\nprint(n);");
    assert!(out.contains("rt_chan_send"), "expected a send:\n{out}");
}

#[test]
fn a_channel_is_shared_rather_than_moved() {
    // The one exemption: a channel is how threads share, so passing it to a
    // spawn aliases it. Without this the ordinary worker pattern could not
    // be written at all.
    let out = ir("void w(Chan<int> c) { send(c, 1); }\nChan<int> c = Chan<int>(2);\nspawn w(c);\nprint(recv(c));");
    assert!(out.contains("spawn w"), "expected the spawn:\n{out}");
}

#[test]
fn channel_misuse_is_rejected() {
    assert!(err("int x = 1;\nsend(x, 2);").contains("needs a channel"));
    assert!(err("int f(int a) { return a; }\nspawn f(1);").contains("needs a void function"));
    assert!(err("Chan<int> c = Chan<int>(2);\nsend(c, \"s\");").contains("expected int"));
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

// ---- collections, for-in, clone ---------------------------------------

#[test]
fn an_array_allocates_once_and_a_list_has_a_buffer() {
    let a = ir("Array<int> a = [0; 4]; print(a[0]);");
    assert!(a.contains("rt_array_new"), "{a}");
    let l = ir("List<int> l = []; l.push(1); print(l[0]);");
    assert!(l.contains("rt_list_new"), "{l}");
}

#[test]
fn indexing_a_reference_element_is_borrowed() {
    // Reading an element does not retain: the collection holds the +1, the
    // same rule as reading a field.
    let out = ir("Array<str> a = [\"\"; 2]; print(a[0]);");
    let main = out.split("func $main").nth(1).unwrap();
    // One retain for the fill argument, and no extra for the read.
    assert_eq!(
        main.matches("rc_inc").count(),
        0,
        "the read must add no refcount traffic:\n{main}"
    );
}

#[test]
fn assigning_an_element_releases_the_old_one() {
    let out = ir("Array<str> a = [\"x\"; 2]; a[0] = \"y\"; print(a[0]);");
    let main = out.split("func $main").nth(1).unwrap();
    assert!(
        main.contains("rt_index_get") && main.contains("rt_index_set"),
        "old value must be read before the store:\n{main}"
    );
    assert!(
        main.contains("rc_dec"),
        "the old element must be released:\n{main}"
    );
}

#[test]
fn for_in_increments_before_the_body() {
    // This is what makes `continue` advance the loop. An increment at the
    // bottom of the body would be skipped by it and the loop would hang.
    let out = ir("List<int> xs = []; xs.push(1); for (int x in xs) { print(x); }");
    let body = out
        .split("block2:")
        .nth(1)
        .expect("expected a loop body block");
    let add = body.find("iadd").expect("the index must be incremented");
    let get = body.find("rt_index_get").expect("the element must be read");
    assert!(
        add < get,
        "the increment must come before the element read:\n{body}"
    );
}

#[test]
fn for_in_reads_the_length_once() {
    let out = ir("List<int> xs = []; for (int x in xs) { print(x); }");
    assert_eq!(
        out.matches("rt_len_of").count(),
        1,
        "the length is read once, before the loop:\n{out}"
    );
}

#[test]
fn clone_copies_a_struct_field_by_field() {
    let out = ir("type P { int x; str s; }\nP a = P(1, \"t\"); P b = clone(a); print(b.x);");
    let main = out.split("func $main").nth(1).unwrap();
    assert!(main.contains("alloc"), "clone must allocate:\n{main}");
    assert!(
        main.matches("store").count() >= 4,
        "both fields must be copied into the new object:\n{main}"
    );
}

#[test]
fn collection_misuse_is_rejected() {
    assert!(err("Array<int> a = Array<int>(4); print(a[0]);").contains("as a literal"));
    assert!(err("Array<int> a = [0; 2]; a.push(1);").contains("needs a List"));
    assert!(err("int x = 1; print(x[0]);").contains("cannot be indexed"));
    assert!(err("List<int> xs = []; xs.push(\"no\");").contains("expected int"));
    assert!(err("int x = 1; print(clone(x));").contains("nothing to clone"));
    assert!(err("List<int> xs = []; for (str s in xs) { print(s); }")
        .contains("expected str, found int"));
}

// ---- bytes -------------------------------------------------------------

#[test]
fn bytes_is_a_type_keyword() {
    assert_eq!(
        toks("bytes b"),
        vec![Tok::KwBytes, Tok::Ident("b".into()), Tok::Eof]
    );
}

#[test]
fn a_byte_is_a_value_and_moves_no_refcount() {
    // Reading, writing and iterating a bytes touch no count: a byte is an
    // int, not a reference, so the only retain is the loop's hold on the
    // collection itself.
    let out = ir("bytes b = [1, 2]; b[0] = b[1]; for (int x in b) { print(x); }");
    let main = out.split("func $main").nth(1).unwrap();
    assert!(
        main.contains("rt_bytes_get") && main.contains("rt_bytes_set"),
        "{main}"
    );
    assert_eq!(
        main.matches("rc_inc").count(),
        1,
        "only the loop's hold on the collection:\n{main}"
    );
}

#[test]
fn a_bytes_literal_builds_through_push() {
    // Sized once for what is written, then filled: each push checks its
    // value, so a computed element traps exactly as `b.push(v)` would.
    let out = ir("int k = 3; bytes b = [1, k]; print(b.size());");
    assert!(out.contains("rt_bytes_new"), "{out}");
    assert_eq!(out.matches("rt_bytes_push").count(), 2, "{out}");
}

// ---- float text ---------------------------------------------------------

/// The module that formats floats is language source the lowering calls into,
/// so a float `to_str` or `print` inside it would be a call to itself.
#[test]
fn the_float_module_cannot_format_a_float_itself() {
    let fm = crate::stdlib::FLOATFMT;
    for body in [
        "str f(float x) { return x.to_str(); }",
        "void f(float x) { print(x); }",
        "bool f(str s) { return s.parse_float().is_some(); }",
    ] {
        let toks = Lexer::new(body).tokenize().expect("lexes");
        let prog = crate::parser::Parser::new(toks)
            .stdlib()
            .parse_program(fm)
            .expect("parses");
        let e = match crate::finish(prog, "c") {
            Ok(_) => panic!("expected the float module to be refused: {body}"),
            Err(d) => d.to_string(),
        };
        assert!(e.contains("cannot format or parse a float itself"), "{e}");
    }
}

/// Without the module loaded, a float conversion names the missing function
/// rather than emitting a call the C compiler would reject.
#[test]
fn a_float_conversion_without_the_module_is_a_named_compiler_bug() {
    assert!(err("print(1.5);").contains("was not loaded for a float conversion"));
}

// ---- code points ----------------------------------------------------------

/// `chars` and `from_chars` are lowered to calls into lib/__text.m31, so the
/// module using either through the method spelling would call itself.
#[test]
fn the_text_module_cannot_use_its_own_conversions() {
    let tm = crate::stdlib::TEXT;
    for body in [
        "List<int> f(str s) { return s.chars(); }",
        "str f(List<int> xs) { return str.from_chars(xs); }",
    ] {
        let toks = Lexer::new(body).tokenize().expect("lexes");
        let prog = crate::parser::Parser::new(toks)
            .stdlib()
            .parse_program(tm)
            .expect("parses");
        let e = match crate::finish(prog, "c") {
            Ok(_) => panic!("expected the text module to be refused: {body}"),
            Err(d) => d.to_string(),
        };
        assert!(
            e.contains("cannot call `chars` or `from_chars` itself"),
            "{e}"
        );
    }
}

/// Without the module loaded, a conversion names the missing function rather
/// than emitting a call the C compiler would reject.
#[test]
fn a_code_point_conversion_without_the_module_is_a_named_compiler_bug() {
    let e = err("List<int> cs = \"a\".chars();");
    assert!(
        e.contains("was not loaded for a code point conversion"),
        "{e}"
    );
    let e = err("List<int> cs = [97]; str s = str.from_chars(cs);");
    assert!(
        e.contains("was not loaded for a code point conversion"),
        "{e}"
    );
}

// ---- `this` ---------------------------------------------------------

#[test]
fn this_is_a_keyword() {
    assert_eq!(toks("this"), vec![Tok::KwThis, Tok::Eof]);
    // Only the whole word: a name that merely starts with it is a name.
    assert_eq!(
        toks("thisone"),
        vec![Tok::Ident("thisone".to_string()), Tok::Eof]
    );
}

#[test]
fn reading_this_is_borrowed_and_returning_it_retains_once() {
    // The receiver is borrowed like a parameter, so using it costs nothing;
    // a return is owned, so `return this;` must add exactly one reference
    // and release none.
    let out = ir("type C { int n = 0; }\n\
                  C C.me() { return this; }\n\
                  print(C().me().n);");
    let me = out
        .split("func ")
        .find(|f| f.starts_with("C.me"))
        .expect("C.me is emitted");
    assert_eq!(me.matches("rc_inc").count(), 1, "{me}");
    assert_eq!(me.matches("rc_dec").count(), 0, "{me}");
}

#[test]
fn the_formatter_prints_this() {
    let src = "type C { int n = 0; }\nC C.me() {\n    return this;\n}\n";
    let out = crate::reformat(src, "t").expect("formats");
    assert!(out.contains("return this;"), "{out}");
}

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

// ---- lambdas --------------------------------------------------------

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

// ---- the caret's display width --------------------------------------

#[test]
fn the_width_tables_are_the_ones_lib_unicode_carries() {
    // src/width.rs has no tables of its own: it parses WIDTH_RANGES and
    // GCB_RANGES out of the embedded text of lib/unicode.m31, so the compiler
    // and the standard library cannot drift apart on what a column is. The
    // counts are the ones the generator recorded in that file's comments; a
    // regeneration for a new Unicode version changes both the numbers here
    // and the prose there, together. A rename or a reshape of either table
    // yields zero, which is what this catches.
    let (width, gcb) = crate::width::table_sizes();
    assert_eq!(width, 494, "WIDTH_RANGES ranges");
    assert_eq!(gcb, 683, "GCB_RANGES ranges");
}

#[test]
fn display_width_measures_clusters_not_code_points() {
    let w = crate::width::display_width;
    assert_eq!(w("abc"), 3);
    // East Asian Wide.
    assert_eq!(w("漢字"), 4);
    // An emoji is wide; a skin-tone modifier joins its cluster and adds none.
    assert_eq!(w("🙂"), 2);
    assert_eq!(w("👍🏽"), 2);
    // A combining mark takes no columns of its own.
    assert_eq!(w("e\u{301}"), 1);
    assert_eq!(w("\u{e9}"), 1);
    // A ZWJ sequence is one cluster and one glyph, measured by its base.
    assert_eq!(w("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}"), 2);
    // A flag is a pair of regional indicators, neutral apart, wide together.
    assert_eq!(w("\u{1F1EC}\u{1F1E7}"), 2);
    // U+FE0F asks for the emoji form, which is two columns.
    assert_eq!(w("1\u{FE0F}\u{20E3}"), 2);
    // An arrow is neutral: one column, not two.
    assert_eq!(w("\u{2192}"), 1);
}
