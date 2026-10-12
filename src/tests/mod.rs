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

use crate::{compile_str, front::lexer::Lexer, front::lexer::Tok};

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

mod arguments;
mod collections;
mod concurrency;
mod control_flow;
mod deps;
mod destructors;
mod diagnostics;
mod generics;
mod init;
mod interfaces;
mod lambdas;
mod lexer;
mod methods;
mod parser;
mod refcount;
mod text;
mod types;
mod width;
