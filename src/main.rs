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
mod fmt;
mod ir;
mod lexer;
mod lower;
mod mono;
mod parser;

#[cfg(test)]
mod tests;

use std::process::ExitCode;

fn usage() -> ExitCode {
    eprintln!("usage: langc --emit-c|--emit-ir|fmt <source> [-o <output>]");
    eprintln!("       langc fmt --check <source>   exit 1 if it is not formatted");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mut mode: Option<&str> = None;
    let mut check = false;
    let mut input: Option<String> = None;
    let mut output: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "fmt" => mode = Some("fmt"),
            "--check" => check = true,
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
                println!("usage: langc --emit-c|--emit-ir|fmt <source> [-o <output>]");
                println!("       langc fmt --check <source>");
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

    if mode == "fmt" {
        let formatted = match reformat(&src) {
            Ok(s) => s,
            Err(d) => {
                eprintln!("{}", d.render_with_source(&path, &src));
                return ExitCode::FAILURE;
            }
        };
        if check {
            if formatted != src {
                eprintln!("{path}: not formatted");
                return ExitCode::FAILURE;
            }
            return ExitCode::SUCCESS;
        }
        if let Err(e) = std::fs::write(&path, formatted) {
            eprintln!("cannot write {path}: {e}");
            return ExitCode::FAILURE;
        }
        return ExitCode::SUCCESS;
    }

    let out = match compile(&src, mode) {
        Ok(s) => s,
        Err(d) => {
            eprintln!("{}", d.render_with_source(&path, &src));
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

/// Format a source file. Parses only -- it must work on a program that does
/// not typecheck, because that is exactly when you reach for the formatter.
fn reformat(src: &str) -> Result<String, diag::Diag> {
    let (toks, comments) = lexer::Lexer::tokenize_with_comments(src)?;
    let prog = parser::Parser::new(toks).parse_program()?;
    fmt::set_type_names(&prog);
    Ok(fmt::format(&prog, comments))
}

fn compile(src: &str, mode: &str) -> Result<String, diag::Diag> {
    let toks = lexer::Lexer::new(src).tokenize()?;
    let prog = parser::Parser::new(toks).parse_program()?;
    // Generics are erased before lowering, which is why the IR has never
    // needed to know about them. See src/mono.rs.
    let prog = mono::Mono::run(prog)?;
    let module = lower::Lowerer::new().lower_program(&prog)?;
    Ok(match mode {
        "ir" => module.to_string(),
        _ => emit_c::emit(&module),
    })
}
