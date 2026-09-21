//! Loading a program from several files. See docs/modules-decision.md.
//!
//! A file is a module and its name is the basename. Imports are acyclic and
//! the check lives here rather than in the type checker -- the dependency
//! graph is not the type checker's business, which is the shape Go settled
//! on after putting the check in three places.
//!
//! Every module is parsed once and the results are concatenated into one
//! `Program`. Past this point nothing downstream knows there were files.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::ast::{Program, TyExpr};
use crate::diag::{Diag, Span};
use crate::lexer::Lexer;
use crate::parser::Parser;

/// A diagnostic plus the file it came from. With several modules a bare
/// line:col does not identify anything.
pub struct Located {
    pub path: String,
    pub diag: Diag,
}

/// The module name a path denotes: its basename without the extension.
pub fn module_name(path: &str) -> String {
    let raw = Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    // Only an IMPORTED module's name has to be an identifier; the entry file
    // is named on the command line and may be called anything. But the name
    // is also the qualifier on every declaration, and that reaches the
    // emitted C -- `022-embedding#Animal` is not a C identifier. Anything
    // that is not one becomes `_`, and a collision with a real module name
    // is caught by the case-fold check like any other.
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Is this usable as a module name?
///
/// An imported file's name becomes a name in the language, so the filename
/// grammar is part of the language grammar for those. Rejected at discovery
/// rather than at the import that first mentions it: there are no warnings
/// here, and a file that can never be named is a mistake wherever it is
/// noticed.
///
/// The ENTRY file is exempt -- it is named on the command line and never in
/// source, so its filename is the shell's business.
fn valid_module_name(n: &str) -> bool {
    !n.is_empty()
        && !n.starts_with(|c: char| c.is_ascii_digit())
        && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

struct Loader {
    dir: PathBuf,
    ext: String,
    /// Parsed modules, by name.
    done: HashMap<String, Program>,
    /// Names already seen, lowercased, to catch case-insensitive collisions
    /// on a case-sensitive filesystem too.
    folded: HashMap<String, String>,
    /// The path each module was read from, for diagnostics.
    paths: HashMap<String, String>,
    /// The DFS stack: reaching a name already on it is the cycle.
    stack: Vec<String>,
    /// ONE interned type arena for the whole program. `Ty::User` indexes it,
    /// so every file has to intern into the same one -- see
    /// `Parser::with_arena`.
    arena: Vec<TyExpr>,
}

/// A loaded program, and where each module was read from, so a diagnostic
/// raised after the merge can still name the right file.
pub struct Loaded {
    pub program: Program,
    pub paths: HashMap<String, String>,
}

/// Load the entry file and everything it imports, in dependency order.
pub fn load(entry: &str) -> Result<Loaded, Located> {
    let entry_path = Path::new(entry);
    let dir = entry_path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let ext = entry_path
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_else(|| "src".to_string());

    let name = module_name(entry);
    let mut l = Loader {
        dir,
        ext,
        done: HashMap::new(),
        folded: HashMap::new(),
        paths: HashMap::new(),
        stack: Vec::new(),
        arena: Vec::new(),
    };
    let order = l.visit(&name, entry, None)?;
    let entry_module = name.clone();

    // Concatenate in dependency order: a module is appended after everything
    // it imports. Declarations are order-independent downstream, but the
    // order makes an emitted C file readable.
    let mut out: Option<Program> = None;
    for name in order {
        let p = l.done.remove(&name).expect("visited");
        match &mut out {
            None => {
                let mut p = p;
                // Dependency order puts the deepest import first, so the
                // accumulator is rarely the entry. Top-level statements
                // belong to the entry module and privacy is checked against
                // it, so the name has to be corrected here.
                p.module = entry_module.clone();
                out = Some(p);
            }
            Some(acc) => {
                // Only the entry file may hold statements -- checked below,
                // so this concatenation cannot reorder anything that runs.
                acc.types.extend(p.types);
                acc.funcs.extend(p.funcs);
                acc.toplevel.extend(p.toplevel);
                // Nothing to merge: every file interned into the loader's
                // single arena, which is put back on the result below.
            }
        }
    }
    let mut out = out.expect("the entry module was visited");
    out.module = entry_module;
    out.ty_exprs = l.arena;
    Ok(Loaded {
        program: out,
        paths: l.paths,
    })
}

impl Loader {
    /// Parse one module and everything it imports, depth first. Returns the
    /// modules in dependency order, imports before importers.
    fn visit(
        &mut self,
        name: &str,
        path: &str,
        from: Option<(&str, Span)>,
    ) -> Result<Vec<String>, Located> {
        // The STACK is checked first. A module is inserted into `done`
        // before recursing -- so that a cycle finds it rather than re-reading
        // the file -- which means checking `done` first swallowed every
        // cycle and let the error surface later as a name resolution
        // failure. Order matters here and the two checks look
        // interchangeable.
        //
        // On the stack: this is the cycle. Print every edge, head repeated to
        // close the loop -- Go keeps the cycle path rather than the shortest
        // path on purpose, and OCaml's missing-symbol-at-link-time is what
        // happens when you ban cycles without a diagnostic that names one.
        if let Some(at) = self.stack.iter().position(|s| s == name) {
            let mut chain = String::new();
            for m in &self.stack[at..] {
                chain.push_str(m);
                chain.push_str("\n    imports ");
            }
            chain.push_str(name);
            let (importer, span) = from.expect("a cycle always has an importer");
            return Err(Located {
                path: self.paths[importer].clone(),
                diag: Diag::new(span, format!("import cycle: {chain}")),
            });
        }

        // Already loaded, and not on the stack: nothing to do.
        if self.done.contains_key(name) {
            return Ok(Vec::new());
        }

        let src = std::fs::read_to_string(path).map_err(|e| {
            let (p, span) = match from {
                Some((importer, span)) => (self.paths[importer].clone(), span),
                None => (path.to_string(), Span::new(1, 1)),
            };
            Located {
                path: p,
                diag: match from {
                    Some(_) => Diag::new(
                        span,
                        format!(
                            "cannot find module `{name}`: no `{name}.{}` beside it",
                            self.ext
                        ),
                    ),
                    None => Diag::new(span, format!("cannot read {path}: {e}")),
                },
            }
        })?;

        let here = |d: Diag| Located {
            path: path.to_string(),
            diag: d,
        };

        // Only an IMPORTED module needs a name that is an identifier, because
        // only an imported module is named in source. The entry file is
        // named on the command line, where the filename grammar is the
        // shell's business -- so `011-types.src` is a fine program and a
        // hopeless import.
        if from.is_some() && !valid_module_name(name) {
            return Err(here(Diag::new(
                Span::new(1, 1),
                format!(
                    "`{name}` is not usable as a module name: a file's name \
                     becomes a name in the language, so it has to be a letter \
                     or `_` followed by letters, digits or `_`"
                ),
            )));
        }

        // Two files whose names differ only in case are refused on EVERY
        // platform, not only where the filesystem would confuse them. Go
        // adopted this rule late and paid for it; it is nearly free now.
        let fold = name.to_ascii_lowercase();
        if let Some(other) = self.folded.get(&fold) {
            if other != name {
                return Err(here(Diag::new(
                    Span::new(1, 1),
                    format!(
                        "module `{name}` collides with `{other}`: two names that \
                         differ only in case are the same file on some systems"
                    ),
                )));
            }
        }
        self.folded.insert(fold, name.to_string());
        self.paths.insert(name.to_string(), path.to_string());

        let toks = Lexer::new(&src).tokenize().map_err(here)?;
        let mut parser = Parser::with_arena(toks, std::mem::take(&mut self.arena));
        let mut prog = parser.parse_program(name).map_err(here)?;
        // `parse_program` hands the arena out with the program, so take it
        // back from there and pass it to the next file. The program's own
        // copy is replaced with the finished arena when the merge is done.
        self.arena = std::mem::take(&mut prog.ty_exprs);

        // Only the entry file runs. Declared rather than discovered: a stray
        // statement in a library would otherwise move the program silently.
        if from.is_some() && !prog.toplevel.is_empty() {
            return Err(here(Diag::new(
                Span::new(1, 1),
                format!(
                    "`{name}` is imported, so it may only declare things: \
                     statements at the top level are the program, and the \
                     program is the file you named"
                ),
            )));
        }

        self.stack.push(name.to_string());
        let mut order = Vec::new();
        let imports: Vec<(String, Span)> = prog
            .imports
            .iter()
            .map(|i| (i.name.clone(), i.span))
            .collect();
        // Insert before recursing so a cycle finds this module on the stack
        // rather than re-reading it.
        self.done.insert(name.to_string(), prog);
        for (dep, span) in imports {
            let p = self.dir.join(format!("{dep}.{}", self.ext));
            let sub = self.visit(&dep, &p.to_string_lossy(), Some((name, span)))?;
            order.extend(sub);
        }
        self.stack.pop();
        order.push(name.to_string());
        Ok(order)
    }
}
