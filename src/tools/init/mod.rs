//! `m31c init`: write the standard project layout (docs/project-layout.md).
//!
//! The files come from templates beside this module, with `@@NAME@@`-style
//! placeholders filled in. `plan` is pure -- it returns every file to write --
//! so the layout can be tested without touching a disk; `main` writes it,
//! and refuses to write anything if a single target already exists.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::resolve::stdlib;

pub const USAGE: &str = "usage: m31c init [--lib] [--name NAME] [--prefix PREFIX] [dir]";

const HELP: &str = "Creates a new project in `dir` (default: the current directory).

  --lib            a library: no entry point, no binary build or release assets
  --name NAME      the project name; lowercase words joined by hyphens
                   (default: the directory's name)
  --prefix PREFIX  the module prefix, as in TUI_text; capital letters and digits
                   (default: the name upcased, or the initials of a hyphenated name)

Nothing is overwritten: if any file it would write already exists, it writes
nothing and lists them.";

pub struct Opts {
    pub name: String,
    pub prefix: String,
    pub lib: bool,
}

pub struct File {
    pub path: String,
    pub text: String,
    pub executable: bool,
}

/// `my-app` -> `my_app`: the part of a name that has to be an identifier.
pub fn ident(name: &str) -> String {
    name.replace('-', "_")
}

pub fn default_prefix(name: &str) -> String {
    let words: Vec<&str> = name.split('-').collect();
    if words.len() > 1 {
        words
            .iter()
            .filter_map(|w| w.chars().next())
            .collect::<String>()
            .to_uppercase()
    } else {
        name.to_uppercase()
    }
}

fn valid_name(name: &str) -> Result<(), String> {
    let bad = name.is_empty()
        || !name.starts_with(|c: char| c.is_ascii_lowercase())
        || name.ends_with('-')
        || name.contains("--")
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if bad {
        return Err(format!(
            "`{name}` is not a project name: use lowercase words joined by hyphens, \
             starting with a letter (my-app)"
        ));
    }
    if stdlib::embedded(&ident(name)).is_some() {
        return Err(format!(
            "`{name}` is a standard library module; pick another name (or --name) \
             so the project's own files do not collide with it"
        ));
    }
    Ok(())
}

fn valid_prefix(prefix: &str) -> Result<(), String> {
    let bad = !prefix.starts_with(|c: char| c.is_ascii_uppercase())
        || !prefix
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
    if bad {
        return Err(format!(
            "`{prefix}` is not a module prefix: use capital letters and digits, \
             starting with a letter (TUI, MDV)"
        ));
    }
    Ok(())
}

fn fill(template: &str, o: &Opts) -> String {
    template
        .replace("@@NAME@@", &o.name)
        .replace("@@IDENT@@", &ident(&o.name))
        .replace("@@PREFIX@@", &o.prefix)
        .replace("@@VERSION@@", "0.1.0")
        .replace("@@M31_REF@@", concat!("v", env!("CARGO_PKG_VERSION")))
}

/// Every file of a new project. Errors on a bad name or prefix.
pub fn plan(o: &Opts) -> Result<Vec<File>, String> {
    valid_name(&o.name)?;
    valid_prefix(&o.prefix)?;
    let mut files = Vec::new();
    let mut add = |path: String, template: &str, executable: bool| {
        files.push(File {
            path,
            text: fill(template, o),
            executable,
        });
    };

    let deps = "# @@NAME@@ project file: this directory is the project root.\n\
                name @@NAME@@\nversion @@VERSION@@\n";
    add("deps".into(), deps, false);
    add(
        "README.md".into(),
        if o.lib {
            include_str!("templates/README-lib.md")
        } else {
            include_str!("templates/README-app.md")
        },
        false,
    );
    add(
        "RENAMES.md".into(),
        include_str!("templates/RENAMES.md"),
        false,
    );
    add(
        ".gitignore".into(),
        include_str!("templates/gitignore"),
        false,
    );
    if !o.lib {
        add(
            format!("{}.m31", ident(&o.name)),
            include_str!("templates/entry.m31"),
            false,
        );
        add(
            "scripts/build.sh".into(),
            include_str!("templates/build.sh"),
            true,
        );
    }
    add(
        format!("core/{}_core.m31", o.prefix),
        include_str!("templates/core.m31"),
        false,
    );
    add(
        "tests/t_core.m31".into(),
        include_str!("templates/t_core.m31"),
        false,
    );

    // The one script that differs between the two kinds of project is
    // tests/test.sh, by a documentation line and a block that builds and runs
    // the binary.
    let (doc, app) = if o.lib {
        ("", "")
    } else {
        (
            "#   program        the built binary, run as a user would run it\n",
            include_str!("templates/test-app.sh.part"),
        )
    };
    let test_sh = include_str!("templates/test.sh")
        .replace("@@TEST_DOC@@\n", doc)
        .replace("@@TEST_APP@@\n", app);
    add("tests/test.sh".into(), &test_sh, true);

    add(
        "docs/design.md".into(),
        include_str!("templates/design.md"),
        false,
    );
    add(
        ".github/workflows/test.yml".into(),
        include_str!("templates/test.yml"),
        false,
    );
    add(
        ".github/workflows/release.yml".into(),
        if o.lib {
            include_str!("templates/release-lib.yml")
        } else {
            include_str!("templates/release-app.yml")
        },
        false,
    );
    Ok(files)
}

fn bad_usage(msg: &str) -> ExitCode {
    eprintln!("{msg}");
    eprintln!("{USAGE}");
    ExitCode::from(2)
}

/// The `init` subcommand. Exit 0 created, 1 could not (files exist, io), 2 bad usage.
pub fn main(args: &[String]) -> ExitCode {
    let mut lib = false;
    let mut name: Option<String> = None;
    let mut prefix: Option<String> = None;
    let mut dir: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--lib" => lib = true,
            "--name" | "--prefix" => {
                let flag = args[i].as_str();
                i += 1;
                let Some(value) = args.get(i) else {
                    return bad_usage(&format!("{flag} needs a value"));
                };
                if flag == "--name" {
                    name = Some(value.clone());
                } else {
                    prefix = Some(value.clone());
                }
            }
            "--help" => {
                println!("{USAGE}\n\n{HELP}");
                return ExitCode::SUCCESS;
            }
            other if other.starts_with('-') => {
                return bad_usage(&format!("unknown option `{other}`"));
            }
            other => {
                if dir.is_some() {
                    return bad_usage("more than one directory");
                }
                dir = Some(other.to_string());
            }
        }
        i += 1;
    }

    let dir = PathBuf::from(dir.unwrap_or_else(|| ".".into()));
    let name = match name {
        Some(n) => n,
        None => match dir_name(&dir) {
            Some(n) => n,
            None => return bad_usage("cannot name the project from this directory: use --name"),
        },
    };
    let opts = Opts {
        prefix: prefix.unwrap_or_else(|| default_prefix(&name)),
        name,
        lib,
    };
    let files = match plan(&opts) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    match write_all(&dir, &files) {
        Ok(()) => {
            let at = dir.display();
            println!(
                "created {} {} in {at} ({} files)",
                if opts.lib { "library" } else { "project" },
                opts.name,
                files.len()
            );
            println!("next: M31_ROOT=<m31 checkout or release> LANGC=<m31c> bash tests/test.sh");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn dir_name(dir: &Path) -> Option<String> {
    // A directory that does not exist yet is named by its last component.
    let named = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    named.file_name().map(|n| n.to_string_lossy().into_owned())
}

/// Writes nothing unless every target is free.
pub fn write_all(dir: &Path, files: &[File]) -> Result<(), String> {
    let taken: Vec<String> = files
        .iter()
        .filter(|f| dir.join(&f.path).exists())
        .map(|f| f.path.clone())
        .collect();
    if !taken.is_empty() {
        return Err(format!(
            "nothing written: {} already exist{} in {}:\n  {}",
            if taken.len() == 1 {
                "this file"
            } else {
                "these files"
            },
            if taken.len() == 1 { "s" } else { "" },
            dir.display(),
            taken.join("\n  ")
        ));
    }
    for f in files {
        let path = dir.join(&f.path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        std::fs::write(&path, &f.text)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        #[cfg(unix)]
        if f.executable {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .map_err(|e| format!("cannot chmod {}: {e}", path.display()))?;
        }
    }
    Ok(())
}
