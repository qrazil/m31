//! Diagnostics. One error, one line, `path:line:col: message`.
//!
//! The corpus compares diagnostics byte-for-byte (corpus/errors/*.err), so
//! this format is part of the language's observable surface, not a detail.
//! Changing it is a breaking change and should come with a decision record.

use std::fmt;

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
}

impl Diag {
    pub fn new(span: Span, msg: impl Into<String>) -> Self {
        Diag {
            span,
            msg: msg.into(),
        }
    }

    /// Render with the path exactly as it was given on the command line, so
    /// the corpus can compare against a stable relative path.
    pub fn render(&self, path: &str) -> String {
        format!(
            "{}:{}:{}: {}",
            path, self.span.line, self.span.col, self.msg
        )
    }
}

impl fmt::Display for Diag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}: {}", self.span.line, self.span.col, self.msg)
    }
}
