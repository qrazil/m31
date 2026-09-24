//! Diagnostics. One error, one line, `path:line:col: message`.
//!
//! The corpus compares diagnostics byte-for-byte (corpus/errors/*.err), so
//! this format is part of the language's observable surface, not a detail.
//! Changing it is a breaking change and should come with a decision record.

use std::fmt;

/// A position in a source file: a 1-based line, and a 1-based column counted
/// in CODE POINTS. Not bytes -- see `Lexer::bump`, which is where the count
/// is kept -- so `line:col` and the caret under the echoed line agree on a
/// line holding non-ASCII text. A tab is one column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub line: u32,
    pub col: u32,
}

impl Span {
    pub fn new(line: u32, col: u32) -> Self {
        Span { line, col }
    }
}

#[derive(Debug, Clone)]
pub struct Diag {
    pub span: Span,
    pub msg: String,
    /// Which module the error is IN, when that is not the file the compiler
    /// was pointed at. A span alone is a line number with no file, and with
    /// several modules that names the wrong source line rather than none.
    pub module: Option<String>,
}

impl Diag {
    pub fn new(span: Span, msg: impl Into<String>) -> Self {
        Diag {
            span,
            msg: msg.into(),
            module: None,
        }
    }

    /// Attribute this to a module, unless it already names one. The innermost
    /// attribution wins: an error raised while lowering a function knows
    /// better than the loop that called it.
    pub fn in_module(mut self, m: &str) -> Self {
        if self.module.is_none() && !m.is_empty() {
            self.module = Some(m.to_string());
        }
        self
    }

    /// Render with the path exactly as it was given on the command line, so
    /// the corpus can compare against a stable relative path.
    pub fn render(&self, path: &str) -> String {
        format!(
            "{}:{}:{}: {}",
            path, self.span.line, self.span.col, self.msg
        )
    }

    /// Render with the offending source line beneath it.
    ///
    /// A location alone makes the reader go and look; showing the line means
    /// the error is legible where it is printed. The caret is a single
    /// column because a span records where a construct starts and not how far
    /// it runs -- widening it means threading end positions through the lexer
    /// and parser, which is worth doing and is not free.
    pub fn render_with_source(&self, path: &str, src: &str) -> String {
        let head = self.render(path);
        let Some(text) = src.lines().nth(self.span.line as usize - 1) else {
            return head;
        };
        let num = self.span.line.to_string();
        let pad = " ".repeat(num.len());
        // A tab is one column (see `Lexer::bump`), so the echoed line renders
        // one as a single space: any other width and the caret, which is
        // padded to the text before it, would not land under the token.
        let shown = text.replace('\t', " ");
        // The COLUMN counts code points, because `line:col` is what an editor
        // is handed and what it jumps to. The CARET is padded by display
        // width, because the line above it is printed raw and a terminal
        // draws it in cells: `漢` takes two of them and a combining mark
        // takes none, so one space per code point lands the caret in the
        // wrong place in both directions. Same rule as `unicode.width`, off
        // the same tables -- see src/width.rs.
        let before: String = shown
            .chars()
            .take(self.span.col.saturating_sub(1) as usize)
            .collect();
        let caret = " ".repeat(crate::width::display_width(&before));
        format!("{head}\n{pad} |\n{num} | {shown}\n{pad} | {caret}^")
    }
}

impl fmt::Display for Diag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}: {}", self.span.line, self.span.col, self.msg)
    }
}
