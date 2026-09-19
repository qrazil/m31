//! langc -- the compiler driver.
//!
//! Usage:
//!     langc --emit-c  <source> -o <out.c>
//!     langc --emit-ir <source> [-o <out.ir>]
//!
//! One error, one line, on stderr, exit 1. The corpus compares diagnostics
//! byte-for-byte, so this is observable surface (src/diag.rs).

mod ast;
mod diag;
mod emit_c;
mod ir;
mod lexer;
mod lower;
mod parser;

#[cfg(test)]
mod tests;

use std::process::ExitCode;

fn usage() -> ExitCode {
    eprintln!("usage: langc --emit-c|--emit-ir <source> [-o <output>]");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mut mode: Option<&str> = None;
    let mut input: Option<String> = None;
    let mut output: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--emit-c" => mode = Some("c"),
            "--emit-ir" => mode = Some("ir"),
            "-o" => {
                i += 1;
                match args.get(i) {
                    Some(p) => output = Some(p.clone()),
                    None => return usage(),
                }
            }
            "--help" => {
                println!("usage: langc --emit-c|--emit-ir <source> [-o <output>]");
                return ExitCode::SUCCESS;
            }
            other if other.starts_with('-') => {
                eprintln!("unknown option `{other}`");
                return ExitCode::from(2);
            }
            other => {
                if input.is_some() {
                    eprintln!("more than one input file");
                    return ExitCode::from(2);
                }
                input = Some(other.to_string());
            }
        }
        i += 1;
    }

    let (Some(mode), Some(path)) = (mode, input) else {
        return usage();
    };

    let src = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let out = match compile(&src, mode) {
        Ok(s) => s,
        Err(d) => {
            eprintln!("{}", d.render(&path));
            return ExitCode::FAILURE;
        }
    };

    match output {
        Some(o) => {
            if let Err(e) = std::fs::write(&o, out) {
                eprintln!("cannot write {o}: {e}");
                return ExitCode::FAILURE;
            }
        }
        None => print!("{out}"),
    }
    ExitCode::SUCCESS
}

fn compile(src: &str, mode: &str) -> Result<String, diag::Diag> {
    let toks = lexer::Lexer::new(src).tokenize()?;
    let prog = parser::Parser::new(toks).parse_program()?;
    let module = lower::Lowerer::new().lower_program(&prog)?;
    Ok(match mode {
        "ir" => module.to_string(),
        _ => emit_c::emit(&module),
    })
}
