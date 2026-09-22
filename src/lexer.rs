//! Hand-written lexer. No table generation, no regex, no dependencies.
//!
//! Type names are keywords (`int`, `bool`, `str`, `bytes`, `void`), as in
//! Oro. That is what lets the parser decide "declaration or expression?" with
//! two tokens of lookahead and no symbol-table feedback -- the thing C needs
//! its lexer hack for. See docs/ir-v0.md §7.3.

use crate::diag::{Diag, Span};

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    // literals and names
    Int(i64),
    Float(f64),
    Str(String),
    Ident(String),

    // type keywords
    KwInt,
    KwBool,
    KwStr,
    KwBytes,
    KwVoid,

    // other keywords
    KwReturn,
    KwIf,
    KwElse,
    KwWhile,
    KwBreak,
    KwContinue,
    KwType,
    KwConst,
    KwInterface,
    KwSpawn,
    KwDistinct,
    KwFloat,
    KwStatic,
    KwPrim,
    KwImport,
    KwPub,
    KwEnum,
    KwMatch,
    KwCase,
    KwFor,
    KwIn,
    KwTrue,
    KwFalse,

    // punctuation
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Semi,
    /// Postfix `?`: propagate a failure. Free as a token because there is no
    /// ternary and there is never going to be one.
    Question,
    Colon,
    Assign,
    Dot,

    // operators
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    EqEq,
    BangEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    AmpAmp,
    PipePipe,
    Bang,
    Amp,
    Pipe,
    Caret,
    Tilde,
    /// `<<`. There is deliberately no `>>` token: `List<List<int>>` ends in
    /// two `>` that close two type argument lists, so the lexer always emits
    /// `>` singly and the parser reads two ADJACENT ones as a right shift in
    /// operator position. `<<` has no such twin -- no valid program has two
    /// `<` in a row -- so it can be one token.
    Shl,

    Eof,
}

impl Tok {
    /// How this token is spelled in a diagnostic.
    pub fn describe(&self) -> String {
        match self {
            Tok::Int(n) => format!("integer `{n}`"),
            Tok::Float(x) => format!("float `{x}`"),
            Tok::Str(_) => "string literal".to_string(),
            Tok::Ident(s) => format!("`{s}`"),
            Tok::Eof => "end of file".to_string(),
            t => format!("`{}`", t.spelling()),
        }
    }

    pub fn spelling(&self) -> &'static str {
        match self {
            Tok::KwInt => "int",
            Tok::KwBool => "bool",
            Tok::KwStr => "str",
            Tok::KwBytes => "bytes",
            Tok::KwVoid => "void",
            Tok::KwReturn => "return",
            Tok::KwIf => "if",
            Tok::KwElse => "else",
            Tok::KwWhile => "while",
            Tok::KwBreak => "break",
            Tok::KwContinue => "continue",
            Tok::KwType => "type",
            Tok::KwConst => "const",
            Tok::KwInterface => "interface",
            Tok::KwSpawn => "spawn",
            Tok::KwDistinct => "distinct",
            Tok::KwFloat => "float",
            Tok::KwStatic => "static",
            Tok::KwPrim => "prim",
            Tok::KwImport => "import",
            Tok::KwPub => "pub",
            Tok::KwEnum => "enum",
            Tok::KwMatch => "match",
            Tok::KwCase => "case",
            Tok::KwFor => "for",
            Tok::KwIn => "in",
            Tok::Colon => ":",
            Tok::Dot => ".",
            Tok::KwTrue => "true",
            Tok::KwFalse => "false",
            Tok::LParen => "(",
            Tok::RParen => ")",
            Tok::LBrace => "{",
            Tok::RBrace => "}",
            Tok::LBracket => "[",
            Tok::RBracket => "]",
            Tok::Comma => ",",
            Tok::Semi => ";",
            Tok::Question => "?",
            Tok::Assign => "=",
            Tok::Plus => "+",
            Tok::Minus => "-",
            Tok::Star => "*",
            Tok::Slash => "/",
            Tok::Percent => "%",
            Tok::EqEq => "==",
            Tok::BangEq => "!=",
            Tok::Lt => "<",
            Tok::LtEq => "<=",
            Tok::Gt => ">",
            Tok::GtEq => ">=",
            Tok::AmpAmp => "&&",
            Tok::PipePipe => "||",
            Tok::Bang => "!",
            Tok::Amp => "&",
            Tok::Pipe => "|",
            Tok::Caret => "^",
            Tok::Tilde => "~",
            Tok::Shl => "<<",
            _ => "token",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Token {
    pub tok: Tok,
    pub span: Span,
}

/// A comment, kept aside so the formatter can put it back.
///
/// Comments are trivia to the parser and invisible in the AST, so a
/// formatter that only walks the AST silently deletes them -- which makes it
/// useless. They are collected here with their position and re-emitted by
/// line.
#[derive(Debug, Clone)]
pub struct Comment {
    pub line: u32,
    pub text: String,
    /// Whether the comment is alone on its line, as opposed to trailing code.
    pub own_line: bool,
}

pub struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    line: u32,
    col: u32,
    comments: Vec<Comment>,
    /// Whether anything but whitespace has been seen on the current line.
    code_on_line: bool,
    /// Each string literal's source text, quotes included, by position.
    spellings: Vec<(Span, String)>,
}

/// Everything the formatter needs from the lexer: the tokens, and what the
/// tokens alone lose -- the comments, and how each string literal was spelled.
pub struct Lexed {
    pub toks: Vec<Token>,
    pub comments: Vec<Comment>,
    pub spellings: Vec<(Span, String)>,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        Lexer {
            src: src.as_bytes(),
            pos: 0,
            line: 1,
            col: 1,
            comments: Vec::new(),
            code_on_line: false,
            spellings: Vec::new(),
        }
    }

    /// Tokenize, and also keep the trivia, for the formatter.
    pub fn tokenize_for_fmt(src: &str) -> Result<Lexed, Diag> {
        let mut lx = Lexer::new(src);
        let toks = lx.run()?;
        Ok(Lexed {
            toks,
            comments: std::mem::take(&mut lx.comments),
            spellings: std::mem::take(&mut lx.spellings),
        })
    }

    fn peek(&self) -> u8 {
        if self.pos < self.src.len() {
            self.src[self.pos]
        } else {
            0
        }
    }

    fn peek2(&self) -> u8 {
        if self.pos + 1 < self.src.len() {
            self.src[self.pos + 1]
        } else {
            0
        }
    }

    fn bump(&mut self) -> u8 {
        let c = self.peek();
        self.pos += 1;
        if c == b'\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        c
    }

    fn here(&self) -> Span {
        Span {
            line: self.line,
            col: self.col,
        }
    }

    /// Skip whitespace and both comment forms. Returns Err on an unterminated
    /// block comment rather than silently eating the rest of the file.
    fn skip_trivia(&mut self) -> Result<(), Diag> {
        loop {
            let c = self.peek();
            if c == b' ' || c == b'\t' || c == b'\r' || c == b'\n' {
                if c == b'\n' {
                    self.code_on_line = false;
                }
                self.bump();
            } else if c == b'/' && self.peek2() == b'/' {
                let at = self.here();
                let own = !self.code_on_line;
                let from = self.pos;
                while self.pos < self.src.len() && self.peek() != b'\n' {
                    self.bump();
                }
                self.comments.push(Comment {
                    line: at.line,
                    text: String::from_utf8_lossy(&self.src[from..self.pos])
                        .trim_end()
                        .to_string(),
                    own_line: own,
                });
            } else if c == b'/' && self.peek2() == b'*' {
                let start = self.here();
                let own = !self.code_on_line;
                let from = self.pos;
                self.bump();
                self.bump();
                loop {
                    if self.pos >= self.src.len() {
                        return Err(Diag::new(start, "unterminated block comment"));
                    }
                    if self.peek() == b'*' && self.peek2() == b'/' {
                        self.bump();
                        self.bump();
                        break;
                    }
                    self.bump();
                }
                self.comments.push(Comment {
                    line: start.line,
                    text: String::from_utf8_lossy(&self.src[from..self.pos]).to_string(),
                    own_line: own,
                });
            } else {
                return Ok(());
            }
        }
    }

    pub fn tokenize(self) -> Result<Vec<Token>, Diag> {
        let mut lx = self;
        lx.run()
    }

    fn run(&mut self) -> Result<Vec<Token>, Diag> {
        let mut out = Vec::new();
        loop {
            self.skip_trivia()?;
            let span = self.here();
            if self.pos >= self.src.len() {
                out.push(Token {
                    tok: Tok::Eof,
                    span,
                });
                return Ok(out);
            }
            self.code_on_line = true;
            let tok = self.next_tok(span)?;
            out.push(Token { tok, span });
        }
    }

    fn next_tok(&mut self, span: Span) -> Result<Tok, Diag> {
        let c = self.peek();

        if c.is_ascii_digit() {
            return self.lex_int(span);
        }
        if c == b'_' || c.is_ascii_alphabetic() {
            return Ok(self.lex_word());
        }
        if c == b'"' {
            return self.lex_str(span);
        }

        self.bump();
        let two = |l: &mut Self, t: Tok| {
            l.bump();
            t
        };
        Ok(match c {
            b'(' => Tok::LParen,
            b')' => Tok::RParen,
            b'{' => Tok::LBrace,
            b'}' => Tok::RBrace,
            b'[' => Tok::LBracket,
            b']' => Tok::RBracket,
            b',' => Tok::Comma,
            b';' => Tok::Semi,
            b'?' => Tok::Question,
            b':' => Tok::Colon,
            b'.' => Tok::Dot,
            b'+' => Tok::Plus,
            b'-' => Tok::Minus,
            b'*' => Tok::Star,
            b'/' => Tok::Slash,
            b'%' => Tok::Percent,
            b'=' if self.peek() == b'=' => two(self, Tok::EqEq),
            b'=' => Tok::Assign,
            b'!' if self.peek() == b'=' => two(self, Tok::BangEq),
            b'!' => Tok::Bang,
            b'<' if self.peek() == b'=' => two(self, Tok::LtEq),
            b'<' if self.peek() == b'<' => two(self, Tok::Shl),
            b'<' => Tok::Lt,
            b'>' if self.peek() == b'=' => two(self, Tok::GtEq),
            b'>' => Tok::Gt,
            b'&' if self.peek() == b'&' => two(self, Tok::AmpAmp),
            b'|' if self.peek() == b'|' => two(self, Tok::PipePipe),
            b'&' => Tok::Amp,
            b'|' => Tok::Pipe,
            b'^' => Tok::Caret,
            b'~' => Tok::Tilde,
            other => {
                return Err(Diag::new(
                    span,
                    format!("unexpected character `{}`", other as char),
                ))
            }
        })
    }

    /// An integer, or a float.
    ///
    /// **A float literal always has a dot with digits on both sides.**
    /// `1.0`, not `1.` and not `.5`. The rule earns its keep twice: `1.` is
    /// hard to distinguish from a member access being typed, and a leading
    /// dot makes `x[.5]` read strangely. An exponent is allowed only after
    /// the dot form -- `1.0e9`, not `1e9` -- so that whether a literal is a
    /// float is decided by one character, not by scanning to the end.
    fn lex_int(&mut self, span: Span) -> Result<Tok, Diag> {
        let start = self.pos;
        while self.peek().is_ascii_digit() || self.peek() == b'_' {
            self.bump();
        }
        if self.peek() == b'.' && self.peek2().is_ascii_digit() {
            self.bump();
            while self.peek().is_ascii_digit() || self.peek() == b'_' {
                self.bump();
            }
            if self.peek() == b'e' || self.peek() == b'E' {
                let save = self.pos;
                self.bump();
                if self.peek() == b'+' || self.peek() == b'-' {
                    self.bump();
                }
                if self.peek().is_ascii_digit() {
                    while self.peek().is_ascii_digit() {
                        self.bump();
                    }
                } else {
                    self.pos = save;
                }
            }
            if self.peek() == b'_' || self.peek().is_ascii_alphabetic() {
                return Err(Diag::new(span, "invalid suffix on float literal"));
            }
            let text: String = std::str::from_utf8(&self.src[start..self.pos])
                .unwrap()
                .chars()
                .filter(|c| *c != '_')
                .collect();
            return match text.parse::<f64>() {
                // Underflow is as much "does not fit" as overflow is. A
                // literal with a nonzero digit that parses to exactly zero
                // has lost the whole value, and saying so is better than
                // silently agreeing the program meant 0.
                Ok(x) if x == 0.0 && text.chars().any(|c| c.is_ascii_digit() && c != '0') => {
                    Err(Diag::new(
                        span,
                        format!("float literal `{text}` is too small for float"),
                    ))
                }
                Ok(x) if x.is_finite() => Ok(Tok::Float(x)),
                // A literal that does not fit is a mistake, not an infinity.
                _ => Err(Diag::new(
                    span,
                    format!("float literal `{text}` does not fit in float"),
                )),
            };
        }
        // A digit run followed immediately by a letter is a typo, not two
        // tokens. Catching it here gives a better message than the parser can.
        if self.peek() == b'_' || self.peek().is_ascii_alphabetic() {
            return Err(Diag::new(span, "invalid suffix on integer literal"));
        }
        let text: String = std::str::from_utf8(&self.src[start..self.pos])
            .unwrap()
            .chars()
            .filter(|c| *c != '_')
            .collect();
        match text.parse::<i64>() {
            Ok(n) => Ok(Tok::Int(n)),
            // int is 64-bit everywhere (docs/ir-v0.md §2.1), so a literal that
            // does not fit is a compile error, not a wrap.
            Err(_) => Err(Diag::new(
                span,
                format!("integer literal `{text}` does not fit in int"),
            )),
        }
    }

    fn lex_word(&mut self) -> Tok {
        let start = self.pos;
        while self.peek() == b'_' || self.peek().is_ascii_alphanumeric() {
            self.bump();
        }
        let w = std::str::from_utf8(&self.src[start..self.pos]).unwrap();
        match w {
            "int" => Tok::KwInt,
            "bool" => Tok::KwBool,
            "str" => Tok::KwStr,
            "bytes" => Tok::KwBytes,
            "void" => Tok::KwVoid,
            "return" => Tok::KwReturn,
            "if" => Tok::KwIf,
            "else" => Tok::KwElse,
            "while" => Tok::KwWhile,
            "break" => Tok::KwBreak,
            "continue" => Tok::KwContinue,
            "type" => Tok::KwType,
            "const" => Tok::KwConst,
            "interface" => Tok::KwInterface,
            "spawn" => Tok::KwSpawn,
            "distinct" => Tok::KwDistinct,
            "float" => Tok::KwFloat,
            "static" => Tok::KwStatic,
            "prim" => Tok::KwPrim,
            "import" => Tok::KwImport,
            "pub" => Tok::KwPub,
            "enum" => Tok::KwEnum,
            "match" => Tok::KwMatch,
            "case" => Tok::KwCase,
            "for" => Tok::KwFor,
            "in" => Tok::KwIn,
            "true" => Tok::KwTrue,
            "false" => Tok::KwFalse,
            _ => Tok::Ident(w.to_string()),
        }
    }

    /// A string literal.
    ///
    /// The escapes are `\\ \" \n \t \r \0`, `\xNN` for one ASCII byte and
    /// `\u{N}` for one Unicode scalar value, written as its UTF-8 bytes
    /// (§1.5). Everything else between the quotes is taken byte for byte,
    /// control characters included -- only a newline ends a literal early.
    ///
    /// A decoded literal is always valid UTF-8: the source is, `\u{}` refuses
    /// surrogates, and `\x` stops at 7F. That last limit is what keeps the
    /// open question of §3.10 open: allowing `\xFF` later is additive, and
    /// forbidding it once programs rely on it is not.
    fn lex_str(&mut self, span: Span) -> Result<Tok, Diag> {
        let from = self.pos;
        // Past the opening quote. Accumulate BYTES, not chars: pushing
        // `byte as char` would decode each UTF-8 continuation byte as its
        // own Latin-1 codepoint and silently mangle any non-ASCII literal.
        self.bump();
        let mut bytes: Vec<u8> = Vec::new();
        loop {
            if self.pos >= self.src.len() {
                return Err(Diag::new(span, "unterminated string literal"));
            }
            let at = self.here();
            match self.bump() {
                b'"' => break,
                b'\n' => return Err(Diag::new(span, "unterminated string literal")),
                b'\\' => {
                    // A backslash as the last thing on the line (or in the
                    // file) leaves the literal open, not a strange escape.
                    if self.pos >= self.src.len() || self.peek() == b'\n' {
                        return Err(Diag::new(span, "unterminated string literal"));
                    }
                    self.escape(at, &mut bytes)?;
                }
                c => bytes.push(c),
            }
        }
        // The formatter prints a literal the way it was written. Decoding is
        // many-to-one -- `"é"`, `"\u{e9}"` and `"\u{00E9}"` are one value --
        // and the spelling is the author's: an escaped BOM or combining accent
        // is written that way precisely so that it can be seen.
        self.spellings.push((
            span,
            String::from_utf8_lossy(&self.src[from..self.pos]).into_owned(),
        ));
        match String::from_utf8(bytes) {
            Ok(s) => Ok(Tok::Str(s)),
            // Unreachable while the source is a Rust `str` and no escape
            // makes a byte past 7F on its own, but a lexer must not panic on
            // a promise another module keeps.
            Err(_) => Err(Diag::new(span, "string literal is not valid UTF-8")),
        }
    }

    /// One escape, the backslash already consumed; `at` is where it began.
    fn escape(&mut self, at: Span, out: &mut Vec<u8>) -> Result<(), Diag> {
        match self.bump() {
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'r' => out.push(b'\r'),
            b'0' => out.push(0),
            b'\\' => out.push(b'\\'),
            b'"' => out.push(b'"'),
            // Exactly two digits. C lets `\x` run on for as many hex digits
            // as follow, so `"\x41BC"` there is one out-of-range character,
            // not `ABC`; a fixed width has no such trap.
            b'x' => {
                let (Some(h), Some(l)) = (hex_digit(self.peek()), hex_digit(self.peek2())) else {
                    return Err(Diag::new(at, "`\\x` needs exactly two hex digits"));
                };
                self.bump();
                self.bump();
                let v = h * 16 + l;
                if v > 0x7f {
                    return Err(Diag::new(
                        at,
                        format!(
                            "`\\x` stops at 7f: a string literal is UTF-8, so write the \
                             character as `\\u{{{v:x}}}`, or raw octets as a `bytes`"
                        ),
                    ));
                }
                out.push(v);
            }
            // Braces rather than C's and Go's fixed-width `\uNNNN` and
            // `\UNNNNNNNN`: one spelling for every plane, and the digits are
            // delimited, so `"\u{e9}9"` cannot be misread.
            b'u' => {
                if self.peek() != b'{' {
                    return Err(Diag::new(at, "`\\u` is written `\\u{...}`, with braces"));
                }
                self.bump();
                let mut v: u32 = 0;
                let mut n = 0;
                while let Some(d) = hex_digit(self.peek()) {
                    if n == 6 {
                        n += 1;
                        break;
                    }
                    self.bump();
                    v = v * 16 + d as u32;
                    n += 1;
                }
                if n == 0 || n > 6 || self.peek() != b'}' {
                    return Err(Diag::new(
                        at,
                        "`\\u{...}` needs one to six hex digits and a closing `}`",
                    ));
                }
                self.bump();
                // `char::from_u32` refuses exactly what UTF-8 cannot encode:
                // the surrogates and anything past U+10FFFF.
                let Some(c) = char::from_u32(v) else {
                    return Err(Diag::new(
                        at,
                        format!("`\\u{{{v:x}}}` is not a Unicode scalar value"),
                    ));
                };
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            other => {
                let shown = if other.is_ascii_graphic() {
                    format!("`\\{}`", other as char)
                } else {
                    "after `\\`".to_string()
                };
                return Err(Diag::new(
                    at,
                    format!(
                        "unknown escape {shown}; the escapes are \\\\ \\\" \\n \\t \\r \\0 \
                         \\xNN and \\u{{N}}"
                    ),
                ));
            }
        }
        Ok(())
    }
}

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Write `s` as a string literal that lexes back to exactly `s`.
///
/// For a literal with no source spelling; the formatter keeps the author's
/// otherwise. It escapes what cannot or should not stand raw between quotes --
/// the quote, the backslash and the ASCII controls, where a raw newline would
/// end the literal and a raw CR or tab cannot be seen -- and writes every
/// other character as itself. The formatter once used Rust's `{:?}`, which
/// writes `\u{1}` and `\u{feff}` in Rust's syntax; the lexer refused them, so
/// formatting a file broke it.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            c if (c as u32) < 0x20 || c == '\x7f' => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
