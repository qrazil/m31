//! m31c -- the compiler driver.
//!
//! Usage (the grammar is Oro's `args`, docs/cli-conventions.md; `m31c --help`
//! is generated from the tables below):
//!     m31c -emit-c  <source> [--out <out.c>]
//!     m31c -emit-ir <source> [--out <out.ir>]
//!     m31c -fmt [--check] [--recursive] <file|dir>...
//!     m31c -lint <file|dir>... [--allow NAME...] [--strict] [--summary]
//!     m31c -init [--lib] [--name N] [--prefix P] [dir]
//!
//! Commands may share a line and run in the order typed. The pre-0.6 spellings
//! (`--emit-c`, `fmt`, `-o`, `-r`, ...) still work for one release: each prints
//! one deprecation line on stderr.
//!
//! One error, one line, on stderr, exit 1. The corpus compares diagnostics
//! byte-for-byte, so this is observable surface (src/front/diag.rs).

mod back;
mod front;
mod middle;
mod resolve;
mod tools;

use back::emit_c;
use front::{ast, diag, lexer, parser};
use middle::{lower, mono};
use resolve::{modules, stdlib};
use tools::cli::{Command, Kind, Need, Operand, Opt, Spec};
use tools::{cli, fmt, init, lint};

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

const OUT: Opt = Opt {
    long: "out",
    short: Some('o'),
    kind: Kind::Value,
    placeholder: "PATH",
    help: "write here instead of standard output",
};

static SOURCE_OPERAND: [Operand; 1] = [Operand {
    name: "source",
    need: Need::Required,
    help: "the .m31 file to compile",
}];

static EMIT_C: Spec = Spec {
    prog: "m31c -emit-c",
    summary: "compile to C",
    opts: &[OUT],
    operands: &SOURCE_OPERAND,
    commands: &[],
    version: None,
    completions: false,
    epilog: "",
};

static EMIT_IR: Spec = Spec {
    prog: "m31c -emit-ir",
    summary: "compile to the IR text",
    opts: &[OUT],
    operands: &SOURCE_OPERAND,
    commands: &[],
    version: None,
    completions: false,
    epilog: "",
};

static FMT_OPERANDS: [Operand; 2] = [
    Operand {
        name: "path",
        need: Need::Required,
        help: "a source file, or with --recursive a directory",
    },
    Operand {
        name: "more",
        need: Need::Tail,
        help: "",
    },
];

static FMT: Spec = Spec {
    prog: "m31c -fmt",
    summary: "format m31 source in place",
    opts: &[
        Opt {
            long: "check",
            short: Some('c'),
            kind: Kind::Flag,
            placeholder: "",
            help: "report unformatted files instead of rewriting them (exit 1)",
        },
        Opt {
            long: "recursive",
            short: Some('r'),
            kind: Kind::Flag,
            placeholder: "",
            help: "format every .m31 file under each directory",
        },
    ],
    operands: &FMT_OPERANDS,
    commands: &[],
    version: None,
    completions: false,
    epilog: "",
};

static ROOT: Spec = Spec {
    prog: "m31c",
    summary: "the m31 compiler",
    opts: &[],
    operands: &[],
    commands: &[
        Command {
            word: "emit-c",
            short: None,
            spec: &EMIT_C,
        },
        Command {
            word: "emit-ir",
            short: None,
            spec: &EMIT_IR,
        },
        Command {
            word: "fmt",
            short: None,
            spec: &FMT,
        },
        Command {
            word: "lint",
            short: None,
            spec: &lint::SPEC,
        },
        Command {
            word: "init",
            short: None,
            spec: &init::SPEC,
        },
    ],
    version: Some(env!("CARGO_PKG_VERSION")),
    completions: true,
    epilog: "Commands share a line and run in the order typed: m31c -fmt --check lib -lint lib.\nOne error, one line, on stderr; exit 0 ok, 1 failure, 2 usage error.\nThe old spellings (--emit-c, fmt, -o, -r, ...) still work for one release and warn.",
};

/// The pre-0.6 spellings, accepted for one release (docs/cli-conventions.md).
const OLD_FIRST_WORDS: &[&str] = &["fmt", "lint", "init"];
const OLD_SPELLINGS: &[(&str, &str)] = &[
    ("--emit-c", "-emit-c"),
    ("--emit-ir", "-emit-ir"),
    ("-o", "--out"),
    ("-r", "--recursive"),
];

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let args = cli::migrate("m31c", &raw, OLD_SPELLINGS, OLD_FIRST_WORDS);
    match cli::parse(&ROOT, &args) {
        Err(message) => cli::fail(&ROOT, &message),
        Ok(cli::Line::Help(text)) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Ok(cli::Line::Version(text)) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Ok(cli::Line::Script(text)) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Ok(cli::Line::Run(commands)) => {
            let mut status = ExitCode::SUCCESS;
            let mut failed = false;
            for (word, parsed) in &commands {
                let code = match word.as_str() {
                    "emit-c" => emit_main("c", &parsed.operands[0], parsed.value("out")),
                    "emit-ir" => emit_main("ir", &parsed.operands[0], parsed.value("out")),
                    "fmt" => fmt_main(parsed),
                    "lint" => lint::run(parsed),
                    _ => init::run(parsed),
                };
                if !failed && code != ExitCode::SUCCESS {
                    status = code;
                    failed = true;
                }
            }
            status
        }
    }
}

/// `-emit-c` and `-emit-ir`: compile `path`, to `out` or standard output.
fn emit_main(mode: &str, path: &str, output: Option<&str>) -> ExitCode {
    if let Err(e) = std::fs::read_to_string(path) {
        eprintln!("cannot read {path}: {e}");
        return ExitCode::FAILURE;
    }
    let out = match compile(path, mode) {
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
            if let Err(e) = std::fs::write(o, out) {
                eprintln!("cannot write {o}: {e}");
                return ExitCode::FAILURE;
            }
        }
        None => print!("{out}"),
    }
    ExitCode::SUCCESS
}

/// `-fmt [--check] [--recursive] <file|dir>...`.
fn fmt_main(parsed: &cli::Parsed) -> ExitCode {
    let check = parsed.flag("check");
    let recursive = parsed.flag("recursive");
    let mut files = Vec::new();
    for path in &parsed.operands {
        if recursive {
            match source_files(Path::new(path)) {
                Ok(found) => files.extend(found),
                Err(e) => {
                    eprintln!("cannot read {path}: {e}");
                    return ExitCode::FAILURE;
                }
            }
        } else {
            files.push(PathBuf::from(path));
        }
    }
    // Every file is attempted, so one run reports everything wrong rather
    // than the first thing.
    let mut ok = true;
    for f in &files {
        ok &= format_file(&f.to_string_lossy(), check);
    }
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Format one file in place, or with `check` only report whether it already is
/// formatted. True when the file is fine (and, without `check`, was written).
fn format_file(path: &str, check: bool) -> bool {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot read {path}: {e}");
            return false;
        }
    };
    let formatted = match reformat(&src, &modules::module_name(path)) {
        Ok(s) => s,
        Err(d) => {
            eprintln!("{}", d.render_with_source(path, &src));
            return false;
        }
    };
    if check {
        if formatted != src {
            eprintln!("{path}: not formatted");
            return false;
        }
        return true;
    }
    // Untouched files keep their modification time, so a recursive run over a
    // formatted tree does not look like a change to anything watching it.
    if formatted != src {
        if let Err(e) = std::fs::write(path, formatted) {
            eprintln!("cannot write {path}: {e}");
            return false;
        }
    }
    true
}

/// Every `.m31` file under `root`, sorted, for `fmt -r`. A file given
/// directly is returned as is. Directories that are not the project's own
/// source are skipped: `.m31-deps` (fetched dependencies), `.git` and
/// `target`. Symbolic links are not followed, so a loop cannot trap the walk.
fn source_files(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if root.is_file() {
        out.push(root.to_path_buf());
        return Ok(out);
    }
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                let skip = matches!(
                    entry.file_name().to_str(),
                    Some(".m31-deps" | ".git" | "target")
                );
                if !skip {
                    dirs.push(path);
                }
            } else if kind.is_file() && path.extension().is_some_and(|e| e == "m31") {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Format a source file. Parses only -- it must work on a program that does
/// not typecheck, because that is exactly when you reach for the formatter.
fn reformat(src: &str, module: &str) -> Result<String, diag::Diag> {
    let lexed = lexer::Lexer::tokenize_for_fmt(src)?;
    let mut p = parser::Parser::new(lexed.toks.clone());
    // The formatter must be able to read the standard library's own source,
    // which is the only source that may write `prim`. Deciding that by module
    // name is right here and nowhere else: this reads a file to print it back,
    // it never compiles it, so a program called `math.m31` gets a formatter
    // that is one keyword too permissive and no more.
    if stdlib::embedded(module).is_some() {
        p = p.stdlib();
    }
    let prog = p.parse_program(module)?;
    fmt::set_type_names(&prog);
    Ok(fmt::format(&prog, lexed, &p.parens))
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
    // needed to know about them. See src/middle/mono/.
    lower::check_destructor_decls(&prog)?;
    lower::check_reserved_decls(&prog)?;
    let prog = mono::Mono::run(prog)?;
    let module = lower::lower_program(&prog)?;
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
