//! Loading a program from several files. See docs/modules-decision.md.
//!
//! A file is a module and its name is the basename. Imports are acyclic and
//! the check lives here rather than in the type checker -- the dependency
//! graph is not the type checker's business, which is the shape Go settled
//! on after putting the check in three places.
//!
//! An import is a path from a project root: `import ui.tuiapp;` is
//! `<root>/ui/tuiapp.m31`, and the module is still named `tuiapp`. See
//! docs/project-layout-decision.md.
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
#[derive(Debug)]
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

/// What a diagnostic calls an embedded module. It is not a path on disk --
/// the source is inside the compiler -- so it is spelt to look like one
/// without pretending to be one.
pub fn display_path(name: &str) -> String {
    format!("<{name}>")
}

/// The text behind a path a diagnostic named, embedded or on disk. One place
/// so that rendering an error in standard library source quotes the right
/// line instead of an empty file.
pub fn read_source(path: &str) -> String {
    if let Some(name) = path.strip_prefix('<').and_then(|p| p.strip_suffix('>')) {
        if let Some(text) = crate::stdlib::embedded(name) {
            return text.to_string();
        }
    }
    std::fs::read_to_string(path).unwrap_or_default()
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

/// Could this file decode a `str` into code points or build one from them?
///
/// Both are spelt with a name no other route reaches -- `s.chars()` and
/// `str.from_chars(xs)` -- so the token is enough. A program that uses
/// `chars` as a name of its own loads the module for nothing, which costs
/// compile time and nothing else.
fn mentions_text(toks: &[crate::lexer::Token]) -> bool {
    use crate::lexer::Tok;
    toks.iter()
        .any(|t| matches!(&t.tok, Tok::Ident(n) if n == "chars" || n == "from_chars"))
}

/// Could this file turn a float into text, or text into a float?
///
/// Every float a program holds was either written as a literal, or has the
/// type `float` spelt somewhere -- a declaration, a parameter, a field, a
/// binding in a `match`, a conversion, `float.from_bits` -- or came out of
/// `parse_float`. There is no type inference that could produce one silently.
/// So a program none of whose files contains one of those tokens has no float
/// to print and no string to parse as one. A wrong answer here in the
/// conservative direction costs compile time; in the other it would be a
/// missing function, which the lowering reports as a compiler bug rather than
/// leaving to the C compiler.
fn mentions_float(toks: &[crate::lexer::Token]) -> bool {
    use crate::lexer::Tok;
    toks.iter().any(|t| match &t.tok {
        Tok::KwFloat | Tok::Float(_) => true,
        Tok::Ident(n) => n == "parse_float",
        _ => false,
    })
}

/// The nearest directory at or above `entry_dir` holding a `deps` manifest:
/// the project root. `deps` is the project file -- present and empty is a
/// project with no dependencies -- so a program's tests and tools can sit in
/// subdirectories and still import the program's modules. The search stops at
/// a repository boundary (`.git`), so a `deps` file somewhere above the
/// checkout cannot reinterpret a program that never asked for one.
///
/// `None` is every program laid out before this existed: no manifest anywhere
/// in sight, and the root is the entry file's own directory, as it always was.
fn find_root(entry_dir: &Path) -> Option<PathBuf> {
    let owned;
    let entry_dir = if entry_dir
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        owned = std::fs::canonicalize(entry_dir).ok()?;
        owned.as_path()
    } else {
        entry_dir
    };
    // Lexical ancestors first, so a relative entry keeps a relative root and
    // diagnostics keep printing the paths the user typed.
    let mut dirs: Vec<PathBuf> = entry_dir.ancestors().map(Path::to_path_buf).collect();
    if entry_dir.is_relative() {
        if let Ok(cwd) = std::env::current_dir() {
            dirs.extend(cwd.ancestors().skip(1).map(Path::to_path_buf));
        }
    }
    for d in dirs {
        if d.join("deps").is_file() {
            return Some(d);
        }
        if d.join(".git").exists() {
            return None;
        }
    }
    None
}

/// Where an import was found, and the root its own imports resolve against.
struct Site {
    path: String,
    root: PathBuf,
    /// Written with a directory in front of the name. A dotted import never
    /// reaches the embedded standard library: `ui.math` is a file or it is
    /// an error, never `math`.
    dotted: bool,
    /// The import as written, `a.b.name`, and what was looked for, for the
    /// "cannot find module" diagnostic.
    shown: String,
    looked_for: String,
}

struct Loader {
    /// The project root: where `deps`, `deps.lock` and `.m31-deps/` live and
    /// where an import in the project's own files starts from.
    root: PathBuf,
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
    /// Whether any file could turn a float into text or text into a float,
    /// and so needs `stdlib::FLOATFMT` -- see `mentions_float`.
    wants_floatfmt: bool,
    /// Whether any file mentions `chars` or `from_chars`, and so needs
    /// `stdlib::TEXT` -- see `mentions_text`.
    wants_text: bool,
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
    let entry_dir = entry_path.parent().unwrap_or(Path::new("."));
    let root = find_root(entry_dir).unwrap_or_else(|| entry_dir.to_path_buf());
    let ext = entry_path
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_else(|| "m31".to_string());

    let name = module_name(entry);
    // The entry file is named on the command line and is never imported, so
    // it escapes the collision check below. It still takes a module name, and
    // `math.m31` as the program would give two different modules called
    // `math` the moment anything imported the real one.
    if crate::stdlib::embedded(&name).is_some() {
        return Err(Located {
            path: entry.to_string(),
            diag: Diag::new(
                Span::new(1, 1),
                format!(
                    "`{name}` is a standard library module, so a program cannot \
                     be called `{name}.{ext}`: module names are globally unique"
                ),
            ),
        });
    }
    let mut l = Loader {
        root,
        ext,
        done: HashMap::new(),
        folded: HashMap::new(),
        paths: HashMap::new(),
        stack: Vec::new(),
        arena: Vec::new(),
        wants_floatfmt: false,
        wants_text: false,
    };
    let entry_site = Site {
        path: entry.to_string(),
        root: l.root.clone(),
        dotted: false,
        shown: name.clone(),
        looked_for: String::new(),
    };
    let mut order = l.visit(&name, entry_site, None)?;
    let entry_module = name.clone();

    // Float text is written in the language (lib/__floatfmt.m31), and the
    // lowering calls into it by name. It is loaded as though the entry file
    // imported it, which it cannot spell, and only when some file could need
    // it: a program with no float in it would otherwise carry, and compile,
    // a few thousand lines it never calls. It goes first, as a dependency
    // would.
    if l.wants_floatfmt {
        let fm = crate::stdlib::FLOATFMT;
        let text = crate::stdlib::embedded(fm).expect("the float module is embedded");
        let extra = l.parse(
            fm,
            &display_path(fm),
            text,
            Some((&name, Span::new(1, 1))),
            true,
            l.root.clone(),
        )?;
        order.splice(0..0, extra);
    }
    // Code points are language source too (lib/__text.m31), loaded on the
    // same terms: only when some file could reach it.
    if l.wants_text {
        let tm = crate::stdlib::TEXT;
        let text = crate::stdlib::embedded(tm).expect("the text module is embedded");
        let extra = l.parse(
            tm,
            &display_path(tm),
            text,
            Some((&name, Span::new(1, 1))),
            true,
            l.root.clone(),
        )?;
        order.splice(0..0, extra);
    }

    // Concatenate in dependency order: a module is appended after everything
    // it imports. Declarations are order-independent downstream, but the
    // order makes an emitted C file readable.
    let mut out: Option<Program> = None;
    let mut imports_by_module = HashMap::new();
    for name in order {
        let p = l.done.remove(&name).expect("visited");
        imports_by_module.insert(
            name.clone(),
            p.imports.iter().map(|i| i.name.clone()).collect::<Vec<_>>(),
        );
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
                // Constants are declarations, so any module may hold them;
                // their values are computed by the compiler, so the order
                // they are concatenated in cannot change anything either.
                acc.consts.extend(p.consts);
                acc.toplevel.extend(p.toplevel);
                // Nothing to merge: every file interned into the loader's
                // single arena, which is put back on the result below.
            }
        }
    }
    let mut out = out.expect("the entry module was visited");
    out.module = entry_module;
    out.imports_by_module = imports_by_module;
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
        site: Site,
        from: Option<(&str, Span)>,
    ) -> Result<Vec<String>, Located> {
        let path = site.path.as_str();

        // A module's name is its identity, so a second file with the same
        // basename -- now possible, since two directories can each hold one --
        // is refused before anything else looks at the name. Checked ahead of
        // the cycle test: `a/util` importing `b/util` would otherwise be
        // reported as `util` importing itself.
        if let Some(prev) = self.paths.get(name) {
            if prev != path && (site.dotted || !prev.starts_with('<')) {
                let (importer, span) = from.expect("only an import can collide");
                let msg = if prev.starts_with('<') {
                    format!(
                        "`{name}` is a standard library module, so {path} \
                         collides with it: module names are globally unique \
                         and there is no way to override one"
                    )
                } else {
                    format!(
                        "module `{name}` is both {prev} and {path}: module \
                         names are globally unique, so two files in different \
                         directories cannot share one -- rename one of them"
                    )
                };
                return Err(Located {
                    path: self.paths[importer].clone(),
                    diag: Diag::new(span, msg),
                });
            }
        }

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

        // The standard library is carried inside the compiler, so an
        // embedded module is found before the filesystem is consulted. A
        // file of the same name beside the program is a COLLISION rather
        // than an override -- module names are globally unique, and `math`
        // is taken.
        if let Some(text) = crate::stdlib::source(name) {
            if std::path::Path::new(path).exists() {
                let (importer, span) = from.expect("an embedded module is always imported");
                return Err(Located {
                    path: self.paths[importer].clone(),
                    diag: Diag::new(
                        span,
                        format!(
                            "`{name}` is a standard library module, so {path} \
                             collides with it: module names are globally unique \
                             and there is no way to override one"
                        ),
                    ),
                });
            }
            if !site.dotted {
                return self.parse(name, &display_path(name), text, from, true, site.root);
            }
        }

        match std::fs::read_to_string(path) {
            Ok(src) => self.parse(name, path, &src, from, false, site.root),
            Err(e) => {
                // The entry file is named on the command line, not imported
                // -- there is no `deps` fallback for it, only for a name some
                // other module actually wrote `import` for. `from` is `None`
                // exactly when this is the entry file.
                let Some((importer, span)) = from else {
                    return Err(Located {
                        path: path.to_string(),
                        diag: Diag::new(Span::new(1, 1), format!("cannot read {path}: {e}")),
                    });
                };

                // Before giving up, a `deps` manifest at the project root
                // may name `name` as a one-file remote git import. This is
                // the fallback between the embedded standard library (above)
                // and the "cannot find module" error (below) -- see
                // docs/remote-imports-decision.md and src/deps.rs. Only the
                // project's own files may reach it: a dependency does not
                // bring dependencies of its own.
                if !site.dotted && site.root == self.root {
                    if let Some(remote_path) = crate::deps::resolve(&self.root, name)? {
                        let remote_str = remote_path.to_string_lossy().into_owned();
                        let remote_src = std::fs::read_to_string(&remote_path).expect(
                            "deps::resolve only ever returns a path it just verified exists",
                        );
                        // Its own imports keep resolving against the project
                        // root, as they always did.
                        return self.parse(
                            name,
                            &remote_str,
                            &remote_src,
                            from,
                            false,
                            self.root.clone(),
                        );
                    }
                }

                let msg = if site.dotted {
                    format!(
                        "cannot find module `{}`: no {}",
                        site.shown, site.looked_for
                    )
                } else {
                    format!(
                        "cannot find module `{name}`: no `{name}.{}` beside it",
                        self.ext
                    )
                };
                Err(Located {
                    path: self.paths[importer].clone(),
                    diag: Diag::new(span, msg),
                })
            }
        }
    }

    /// Where `import a.b.name;` is looked for, from a module whose imports
    /// resolve against `root`.
    ///
    /// Every import is a path from a root, never from the importing file's
    /// own directory, so moving a file does not change what its imports
    /// mean. The root is the project's, or -- for a module that came out of
    /// a directory dependency -- that dependency's checkout.
    fn locate(
        &self,
        segs: &[String],
        root: &Path,
        importer: &str,
        span: Span,
    ) -> Result<Site, Located> {
        let name = segs.last().expect("a path has a segment");
        let shown = segs.join(".");
        if segs.len() == 1 {
            let p = root.join(format!("{name}.{}", self.ext));
            return Ok(Site {
                path: p.to_string_lossy().into_owned(),
                root: root.to_path_buf(),
                dotted: false,
                shown,
                looked_for: String::new(),
            });
        }
        let first = &segs[0];
        // A dependency named in `deps` is a directory of modules, reached
        // through its name: `import tui.geom;`. Only the project's own files
        // can name one -- a dependency brings no dependencies of its own.
        if root == self.root && crate::deps::declares(&self.root, first)? {
            if self.root.join(first).is_dir() {
                return Err(Located {
                    path: self.paths[importer].clone(),
                    diag: Diag::new(
                        span,
                        format!(
                            "`{first}` is both a directory here and a dependency \
                             named in `deps`, so `{shown}` could be either: rename one"
                        ),
                    ),
                });
            }
            let rel = format!("{}.{}", segs[1..].join("/"), self.ext);
            let found = crate::deps::resolve_package(&self.root, first, &rel)?
                .expect("`declares` just said the dependency is named");
            return Ok(Site {
                path: found.to_string_lossy().into_owned(),
                root: crate::deps::package_root(&self.root, first),
                dotted: true,
                shown,
                looked_for: format!("`{rel}` in dependency `{first}`"),
            });
        }
        let rel = format!("{}.{}", segs.join("/"), self.ext);
        let under = if root == self.root {
            "the project root"
        } else {
            "the dependency's root"
        };
        Ok(Site {
            path: root.join(&rel).to_string_lossy().into_owned(),
            root: root.to_path_buf(),
            dotted: true,
            shown,
            looked_for: format!("`{rel}` under {under}"),
        })
    }

    /// Everything that happens once a module's text is in hand, whichever
    /// side of the filesystem it came from. `stdlib` is true only for source
    /// the compiler carries, and is what unlocks `prim` -- see
    /// docs/stdlib-seam.md.
    fn parse(
        &mut self,
        name: &str,
        path: &str,
        src: &str,
        from: Option<(&str, Span)>,
        stdlib: bool,
        root: PathBuf,
    ) -> Result<Vec<String>, Located> {
        let here = |d: Diag| Located {
            path: path.to_string(),
            diag: d,
        };

        // Only an IMPORTED module needs a name that is an identifier, because
        // only an imported module is named in source. The entry file is
        // named on the command line, where the filename grammar is the
        // shell's business -- so `011-types.m31` is a fine program and a
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

        let toks = Lexer::new(src).tokenize().map_err(here)?;
        if mentions_float(&toks) {
            self.wants_floatfmt = true;
        }
        if mentions_text(&toks) {
            self.wants_text = true;
        }
        let mut parser = Parser::with_arena(toks, std::mem::take(&mut self.arena));
        if stdlib {
            parser = parser.stdlib();
        }
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
        let imports: Vec<(Vec<String>, Span)> = prog
            .imports
            .iter()
            .map(|i| (i.path.clone(), i.span))
            .collect();
        // Insert before recursing so a cycle finds this module on the stack
        // rather than re-reading it.
        self.done.insert(name.to_string(), prog);
        for (segs, span) in imports {
            let site = self.locate(&segs, &root, name, span)?;
            let dep = segs.last().expect("a path has a segment").clone();
            let sub = self.visit(&dep, site, Some((name, span)))?;
            order.extend(sub);
        }
        self.stack.pop();
        order.push(name.to_string());
        Ok(order)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn scratch() -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "m31-modules-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, rel: &str, text: &str) -> String {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, text).unwrap();
        p.to_string_lossy().into_owned()
    }

    const LIB: &str = "pub int one() {\n    return 1;\n}\n";

    #[test]
    fn a_deps_file_above_the_entry_makes_its_directory_the_root() {
        let d = scratch();
        write(&d, "deps", "# no dependencies\n");
        write(&d, "repo.m31", LIB);
        let t = write(
            &d,
            "tests/t.m31",
            "import repo;\nimport util.more;\nprint(str(repo.one() + more.two()));\n",
        );
        write(
            &d,
            "util/more.m31",
            "import repo;\npub int two() {\n    return repo.one() + 1;\n}\n",
        );
        let loaded = load(&t).unwrap_or_else(|e| panic!("{}", e.diag.msg));
        assert!(loaded.paths.contains_key("repo"));
        assert!(loaded.paths.contains_key("more"));
    }

    #[test]
    fn without_a_deps_file_the_root_is_the_entry_directory() {
        let d = scratch();
        write(&d, "repo.m31", LIB);
        let t = write(&d, "tests/t.m31", "import repo;\nprint(str(repo.one()));\n");
        let err = load(&t).err().expect("repo.m31 is not beside the entry");
        assert!(err.diag.msg.contains("cannot find module `repo`"));
    }

    #[test]
    fn the_search_for_deps_stops_at_a_repository_boundary() {
        let d = scratch();
        write(&d, "deps", "");
        std::fs::create_dir_all(d.join("proj/.git")).unwrap();
        write(&d, "proj/repo.m31", LIB);
        let t = write(
            &d,
            "proj/tests/t.m31",
            "import repo;\nprint(str(repo.one()));\n",
        );
        assert!(find_root(&d.join("proj/tests")).is_none());
        assert!(load(&t).is_err());
    }

    #[test]
    fn the_nearest_deps_file_wins() {
        let d = scratch();
        write(&d, "deps", "");
        write(&d, "inner/deps", "");
        assert_eq!(find_root(&d.join("inner/tests")), Some(d.join("inner")));
        assert_eq!(find_root(&d.join("tests")), Some(d.clone()));
    }

    #[test]
    fn imports_resolve_from_the_root_not_from_the_importing_file() {
        let d = scratch();
        write(&d, "deps", "");
        write(
            &d,
            "a/x.m31",
            "import a.y;\npub int f() {\n    return y.g();\n}\n",
        );
        write(&d, "a/y.m31", "pub int g() {\n    return 7;\n}\n");
        let t = write(&d, "main.m31", "import a.x;\nprint(str(x.f()));\n");
        assert!(load(&t).is_ok());
        // `y` alone would be `<root>/y.m31`, which is not there.
        write(
            &d,
            "a/x.m31",
            "import y;\npub int f() {\n    return y.g();\n}\n",
        );
        let err = load(&t).err().expect("a sibling is not found by name");
        assert!(err.diag.msg.contains("cannot find module `y`"));
    }
}
