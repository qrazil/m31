use super::*;

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
