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
mod modules;
mod mono;
mod parser;
mod stdlib;

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
        let formatted = match reformat(&src, &modules::module_name(&path)) {
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

    let out = match compile(&path, mode) {
        Ok(s) => s,
        Err(modules::Located { path: p, diag }) => {
            // The diagnostic has to name the file it came from: with several
            // modules, a bare line:col is not enough to find anything.
            let text = modules::read_source(&p);
            eprintln!("{}", diag.render_with_source(&p, &text));
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
fn reformat(src: &str, module: &str) -> Result<String, diag::Diag> {
    let (toks, comments) = lexer::Lexer::tokenize_with_comments(src)?;
    let toks_copy = toks.clone();
    let mut p = parser::Parser::new(toks);
    // The formatter must be able to read the standard library's own source,
    // which is the only source that may write `prim`. Deciding that by module
    // name is right here and nowhere else: this reads a file to print it back,
    // it never compiles it, so a program called `math.src` gets a formatter
    // that is one keyword too permissive and no more.
    if stdlib::embedded(module).is_some() {
        p = p.stdlib();
    }
    let prog = p.parse_program(module)?;
    fmt::set_type_names(&prog);
    let ends = item_extents(&toks_copy, &prog);
    Ok(fmt::format(&prog, comments, ends))
}

/// Where each top-level declaration ends, by line, keyed by where it starts.
///
/// The AST records where a declaration begins but not where it ends, and the
/// formatter needs both: a comment between the two is inside the body and
/// must stay there when declarations are reordered. The token stream has the
/// answer -- the brace that closes the first one opened, or the `;` of a
/// declaration with no body.
fn item_extents(toks: &[lexer::Token], prog: &ast::Program) -> std::collections::HashMap<u32, u32> {
    use lexer::Tok;
    let mut starts: Vec<(u32, bool)> = Vec::new();
    for t in &prog.types {
        starts.push((t.span.line, t.distinct_base.is_some()));
    }
    for f in &prog.funcs {
        starts.push((f.span.line, f.is_prim));
    }
    let mut out = std::collections::HashMap::new();
    for (line, bodiless) in starts {
        let Some(mut i) = toks.iter().position(|t| t.span.line >= line) else {
            continue;
        };
        let mut depth = 0i32;
        while i < toks.len() {
            match toks[i].tok {
                Tok::Semi if bodiless && depth == 0 => break,
                Tok::LBrace => depth += 1,
                Tok::RBrace => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        if let Some(t) = toks.get(i) {
            out.insert(line, t.span.line);
        }
    }
    out
}

fn compile(entry: &str, mode: &str) -> Result<String, modules::Located> {
    // One program, assembled from however many files it imports. Cycles are
    // refused here -- see docs/modules-decision.md.
    let loaded = modules::load(entry)?;
    let paths = loaded.paths;
    finish(loaded.program, mode).map_err(|d| {
        // A diagnostic raised after the merge knows which MODULE it is in,
        // not which file -- so resolve it here. Without this every type,
        // name and privacy error was blamed on the entry file, quoting an
        // innocent line at the same number.
        let path = d
            .module
            .as_ref()
            .and_then(|m| paths.get(m))
            .cloned()
            .unwrap_or_else(|| entry.to_string());
        modules::Located { path, diag: d }
    })
}

/// Everything after the source has been assembled into one program. Split out
/// so the unit tests can drive it from a string without touching the
/// filesystem -- they test the compiler, not the loader.
fn finish(prog: ast::Program, mode: &str) -> Result<String, diag::Diag> {
    // Generics are erased before lowering, which is why the IR has never
    // needed to know about them. See src/mono.rs.
    let prog = mono::Mono::run(prog)?;
    let module = lower::Lowerer::new().lower_program(&prog)?;
    Ok(match mode {
        "ir" => module.to_string(),
        _ => emit_c::emit(&module),
    })
}

/// Compile one source string as a single-module program.
///
/// The module name is empty, which turns module qualification off: with one
/// module there is nothing to collide with, and the tests then assert on the
/// names someone actually wrote. Real programs always go through the loader
/// and are always qualified, so the qualified path is what the corpus
/// exercises.
#[cfg(test)]
fn compile_str(src: &str, mode: &str) -> Result<String, diag::Diag> {
    let toks = lexer::Lexer::new(src).tokenize()?;
    let prog = parser::Parser::new(toks).parse_program("")?;
    finish(prog, mode)
}
