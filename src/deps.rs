//! Remote imports: one `deps` manifest line resolved to a git checkout. See
//! docs/remote-imports-decision.md for the design, and `Loader::visit` in
//! src/modules.rs for the one call site -- this is the single new fallback
//! between "check the embedded standard library" and "error: cannot find
//! module", nothing more.
//!
//! Everything here shells out to the system `git` via `std::process::Command`
//! rather than linking a library: `Cargo.toml`'s `[dependencies]` block is
//! enforced empty by `gates.sh`'s "no dependencies" gate, and a subprocess is
//! the only way to reach git without one. That also makes `git` itself a new,
//! explicit requirement for compiling a program that uses a remote import --
//! and nothing at all for one that doesn't, since this module is never
//! consulted unless a `deps` file exists and names the module being looked
//! for.
//!
//! Two files, beside the entry file, at the one place local imports already
//! look (`Loader.dir`, not a new "project root"):
//!
//! - `deps` -- the project file. A header (`name <project>`, `version
//!   <x.y.z>`, both required, like a `Cargo.toml`'s `[package]`) and then
//!   intent, one dependency per line: `name url ref` for a git checkout or
//!   `name path <dir>` for a directory used in place (no fetch, no lock line).
//! - `deps.lock` -- reality: `name commit-sha`, appended once per name and
//!   never rewritten after. A locked commit is authoritative forever; the
//!   only way to move it is to delete its line by hand and let the next
//!   build re-resolve `ref`.
//!
//! The cache is `.m31-deps/<name>/`, also beside the entry file -- a real
//! `git` checkout at the locked commit, not a global `~/.cargo`-style shared
//! cache (see the decision doc's §2 for why: this build has no other
//! environment-dependent state today, and a shared cache is exactly what Go's
//! GOPATH era spent years cleaning up after).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::diag::{Diag, Span};
use crate::modules::Located;

/// One parsed, non-comment, non-blank line of `deps` or `deps.lock`: the
/// fields after the name, and the line it came from, so a diagnostic about a
/// bad entry can point at it directly rather than at whichever import
/// statement happened to ask for it.
struct ManifestLine {
    fields: Vec<String>,
    line: u32,
}

/// A line of text with a file and a line number, built into a `Located` the
/// same shape every other diagnostic in this compiler uses -- so an error
/// raised here prints exactly like `src/modules.rs`'s "cannot find module",
/// not a different shape bolted on beside it.
fn located(path: &Path, line: u32, msg: String) -> Located {
    Located {
        path: path.to_string_lossy().into_owned(),
        diag: Diag::new(Span::new(line, 1), msg),
    }
}

/// Where one dependency comes from.
enum Source {
    /// `name url ref`: a git checkout under `.m31-deps/<name>/`, pinned by
    /// `deps.lock`.
    Git { url: String, git_ref: String },
    /// `name path <dir>`: a directory used in place, relative to the manifest.
    Path(String),
}

struct Dep {
    source: Source,
    line: u32,
}

/// A parsed `deps` file: the project it names and what that project uses.
/// The header is required so that a project with no dependencies still has
/// content -- an empty file says nothing about what it is the root of.
pub struct Manifest {
    #[allow(dead_code)]
    pub name: String,
    #[allow(dead_code)]
    pub version: String,
    deps: HashMap<String, Dep>,
}

/// `[A-Za-z0-9_-]+`. Informational only: the name never reaches an import.
fn valid_project_name(n: &str) -> bool {
    !n.is_empty()
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// `major.minor.patch`, each all digits, with an optional `-pre` and/or
/// `+build` suffix of `[0-9A-Za-z.-]`.
fn valid_version(v: &str) -> bool {
    let core_end = v.find(['-', '+']).unwrap_or(v.len());
    let (core, rest) = v.split_at(core_end);
    let nums: Vec<&str> = core.split('.').collect();
    nums.len() == 3
        && nums
            .iter()
            .all(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        && rest
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
}

/// Parse a `deps` file. Every line is checked, including ones nothing has
/// asked about yet -- see `parse_manifest` for why -- and then the header.
fn parse_deps(path: &Path, text: &str) -> Result<Manifest, Located> {
    let mut name: Option<String> = None;
    let mut version: Option<String> = None;
    let mut deps = HashMap::new();
    for (i, raw) in text.lines().enumerate() {
        let line_no = (i + 1) as u32;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        match fields[0] {
            key @ ("name" | "version") => {
                if fields.len() != 2 {
                    return Err(located(
                        path,
                        line_no,
                        format!(
                            "malformed `{key}` line: expected `{key} <{}>` -- \
                             `name` and `version` are the project header, so a \
                             dependency cannot be called `{key}`",
                            if key == "name" {
                                "project-name"
                            } else {
                                "x.y.z"
                            }
                        ),
                    ));
                }
                let slot = if key == "name" {
                    &mut name
                } else {
                    &mut version
                };
                if slot.is_some() {
                    return Err(located(
                        path,
                        line_no,
                        format!("duplicate `{key}` line: a `deps` file has exactly one"),
                    ));
                }
                if key == "name" && !valid_project_name(fields[1]) {
                    return Err(located(
                        path,
                        line_no,
                        format!(
                            "`{}` is not a project name: use letters, digits, `-` \
                             and `_`",
                            fields[1]
                        ),
                    ));
                }
                if key == "version" && !valid_version(fields[1]) {
                    return Err(located(
                        path,
                        line_no,
                        format!(
                            "`{}` is not a version: write it as `major.minor.patch`, \
                             for example `0.1.0`",
                            fields[1]
                        ),
                    ));
                }
                *slot = Some(fields[1].to_string());
            }
            dep_name => {
                if fields.len() != 3 {
                    return Err(located(
                        path,
                        line_no,
                        format!(
                            "malformed `deps` line: expected 3 whitespace-separated \
                             fields, found {}",
                            fields.len()
                        ),
                    ));
                }
                let source = if fields[1] == "path" {
                    Source::Path(fields[2].to_string())
                } else {
                    Source::Git {
                        url: fields[1].to_string(),
                        git_ref: fields[2].to_string(),
                    }
                };
                if deps
                    .insert(
                        dep_name.to_string(),
                        Dep {
                            source,
                            line: line_no,
                        },
                    )
                    .is_some()
                {
                    return Err(located(
                        path,
                        line_no,
                        format!("duplicate dependency `{dep_name}`"),
                    ));
                }
            }
        }
    }
    match (name, version) {
        (Some(name), Some(version)) => Ok(Manifest {
            name,
            version,
            deps,
        }),
        (None, None) => Err(located(
            path,
            1,
            "`deps` has no project header: add `name <project-name>` and \
             `version <x.y.z>` at the top, for example `name myapp` and \
             `version 0.1.0`"
                .to_string(),
        )),
        (Some(_), None) => Err(located(
            path,
            1,
            "`deps` is missing its `version` line: add `version 0.1.0` (or the \
             project's own version) below `name`"
                .to_string(),
        )),
        (None, Some(_)) => Err(located(
            path,
            1,
            "`deps` is missing its `name` line: add `name <project-name>` above \
             `version`"
                .to_string(),
        )),
    }
}

/// The `deps` manifest in `dir`, parsed and validated, or `None` when there
/// is no `deps` file there. A file that exists but has no valid header is an
/// error, never a silent "no dependencies".
pub fn read_manifest(dir: &Path) -> Result<Option<Manifest>, Located> {
    let deps_path = dir.join("deps");
    let Ok(text) = std::fs::read_to_string(&deps_path) else {
        return Ok(None);
    };
    parse_deps(&deps_path, &text).map(Some)
}

/// `dir/rel` with `.` and `..` folded away lexically (no filesystem access,
/// so a relative path stays relative), so a path dependency's files print as
/// `apps/tui/tuibuf.m31` rather than `apps/git/../tui/tuibuf.m31`.
fn clean(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Parse `deps` or `deps.lock`'s shared shape: `name` followed by exactly
/// `arity` more whitespace-separated fields, one entry per line, blank lines
/// and `#`-comments ignored. `noun` is only for the error message, so a bad
/// `deps.lock` line is not reported as a bad `deps` line.
///
/// A malformed line anywhere in the file is refused eagerly, even one for a
/// name this particular resolution does not need: both files are manifests
/// that either describe a coherent set of remote imports or don't, and a
/// typo three lines down from the one actually in use is still worth
/// catching before it is trusted silently later.
fn parse_manifest(
    path: &Path,
    text: &str,
    arity: usize,
    noun: &str,
) -> Result<HashMap<String, ManifestLine>, Located> {
    let mut out = HashMap::new();
    for (i, raw) in text.lines().enumerate() {
        let line_no = (i + 1) as u32;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() != arity + 1 {
            return Err(located(
                path,
                line_no,
                format!(
                    "malformed `{noun}` line: expected {} whitespace-separated \
                     fields, found {}",
                    arity + 1,
                    fields.len()
                ),
            ));
        }
        out.insert(
            fields[0].to_string(),
            ManifestLine {
                fields: fields[1..].iter().map(|s| s.to_string()).collect(),
                line: line_no,
            },
        );
    }
    Ok(out)
}

/// A `deps` line's `url` field, resolved against `dir` (the directory the
/// `deps` file itself lives in) when it looks like a plain filesystem path
/// rather than something `git` already knows how to resolve on its own --
/// a URL scheme (`https://`, `git://`, `ssh://`, `file://`, ...), scp-like
/// shorthand (`user@host:path`), or an already-absolute path.
///
/// This is not in the decision doc, which only says `ref` is "anything `git
/// checkout` accepts" and leaves `url` at "a git location". A relative path
/// is ambiguous on its own -- relative to what? -- and resolving it against
/// the compiler's current working directory would make a `deps` file's
/// meaning depend on where `m31c` happened to be invoked from, which no
/// other path in this feature does: `deps`, `deps.lock` and `.m31-deps/` are
/// all beside the entry file regardless of invocation directory, so `url`
/// is resolved the same way. This is also what lets a test fixture commit a
/// local repository beside its `deps` file and name it with a path that
/// works no matter where the corpus is checked out.
fn resolve_url(dir: &Path, url: &str) -> String {
    if url.contains("://") || url.contains('@') || Path::new(url).is_absolute() {
        url.to_string()
    } else {
        dir.join(url).to_string_lossy().into_owned()
    }
}

/// Why a `git` subprocess could not be run at all, as opposed to running and
/// failing -- the two need different messages, since one names a missing
/// program and the other quotes what the program itself said.
enum GitFailure {
    NotFound,
    Io(String),
}

fn run_git(args: &[&str], cwd: Option<&Path>) -> Result<std::process::Output, GitFailure> {
    let mut cmd = std::process::Command::new("git");
    cmd.args(args);
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    cmd.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            GitFailure::NotFound
        } else {
            GitFailure::Io(e.to_string())
        }
    })
}

fn git_failure_located(f: GitFailure, path: &Path, line: u32, name: &str) -> Located {
    match f {
        GitFailure::NotFound => located(
            path,
            line,
            format!(
                "cannot resolve remote import `{name}`: `git` was not found on \
                 PATH -- fetching a `deps` entry requires the system git binary \
                 to be installed"
            ),
        ),
        GitFailure::Io(e) => located(
            path,
            line,
            format!("cannot run git to resolve remote import `{name}`: {e}"),
        ),
    }
}

/// Does `cache_dir` already hold a checkout of exactly `commit`? False for
/// anything else -- missing, not a git repository, or checked out to a
/// different commit -- which is deliberately the same answer for all three:
/// every one of them means "reclone and check out the lock again", never a
/// silent patch-up of whatever is sitting there.
fn cache_matches(cache_dir: &Path, commit: &str) -> bool {
    if !cache_dir.is_dir() {
        return false;
    }
    match run_git(&["rev-parse", "HEAD"], Some(cache_dir)) {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim() == commit,
        _ => false,
    }
}

/// Fetch `name` fresh into `cache_dir`, discarding whatever was there. Used
/// both for a first-ever resolution and for repairing a cache that no longer
/// matches its lock -- in both cases the right move is the same clean clone,
/// never an attempt to patch an existing checkout in place.
fn reclone(
    url: &str,
    cache_dir: &Path,
    deps_path: &Path,
    line: u32,
    name: &str,
) -> Result<(), Located> {
    let _ = std::fs::remove_dir_all(cache_dir);
    if let Some(parent) = cache_dir.parent() {
        // Ignored: if this fails, the `git clone` just below fails too, with
        // its own, more specific message.
        let _ = std::fs::create_dir_all(parent);
    }
    let dest = cache_dir.to_string_lossy().into_owned();
    let out = run_git(&["clone", "--quiet", url, &dest], None)
        .map_err(|f| git_failure_located(f, deps_path, line, name))?;
    if !out.status.success() {
        return Err(located(
            deps_path,
            line,
            format!(
                "cannot clone remote import `{name}` from {url}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ));
    }
    Ok(())
}

/// First resolution only: check out `ref` -- a branch, tag, or commit, per
/// the decision doc -- never consulted again once `deps.lock` has a line for
/// `name`.
fn checkout_ref(
    cache_dir: &Path,
    git_ref: &str,
    deps_path: &Path,
    line: u32,
    name: &str,
) -> Result<(), Located> {
    let out = run_git(&["checkout", "--quiet", git_ref], Some(cache_dir))
        .map_err(|f| git_failure_located(f, deps_path, line, name))?;
    if !out.status.success() {
        return Err(located(
            deps_path,
            line,
            format!(
                "cannot check out `{git_ref}` for remote import `{name}`: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ));
    }
    Ok(())
}

/// A stale or missing cache being repaired: check out the LOCKED commit, not
/// `ref` -- the lock already won by the time this runs, so a failure here is
/// reported against `deps.lock`'s own line, not `deps`'s. This is the "a
/// `deps.lock` entry naming a commit `.m31-deps/<name>/` cannot be made to
/// check out to" error from the decision doc -- a rewritten remote history,
/// most likely.
fn checkout_locked_commit(
    cache_dir: &Path,
    commit: &str,
    lock_path: &Path,
    line: u32,
    name: &str,
) -> Result<(), Located> {
    let out = run_git(&["checkout", "--quiet", commit], Some(cache_dir))
        .map_err(|f| git_failure_located(f, lock_path, line, name))?;
    if !out.status.success() {
        return Err(located(
            lock_path,
            line,
            format!(
                "`deps.lock` names commit {commit} for `{name}`, but checking it \
                 out failed: {} -- the remote history this was resolved against \
                 may have been rewritten",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ));
    }
    Ok(())
}

fn rev_parse_head(
    cache_dir: &Path,
    deps_path: &Path,
    line: u32,
    name: &str,
) -> Result<String, Located> {
    let out = run_git(&["rev-parse", "HEAD"], Some(cache_dir))
        .map_err(|f| git_failure_located(f, deps_path, line, name))?;
    if !out.status.success() {
        return Err(located(
            deps_path,
            line,
            format!(
                "cannot resolve the commit remote import `{name}` was checked \
                 out to: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Append `name commit` to `deps.lock`, creating the file if this is its
/// first line. Never a rewrite of an existing line -- see the module
/// doc comment -- so this is only ever called once a lookup has already
/// established `name` has no line yet.
fn append_lock(lock_path: &Path, name: &str, commit: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(lock_path)?;
    writeln!(f, "{name} {commit}")
}

/// The module shape from the decision doc's §2: one file, `<name>.m31`, at
/// the repository's root -- hardcoded, not the entry file's own extension,
/// because a remote import's shape is a property of the feature, not of
/// whatever file happened to start the build that pulled it in.
///
/// A dependency imported by a dotted path (`import tui.geom;`) is a whole
/// directory instead, and `package` is the file asked of it inside the
/// checkout, e.g. `geom.m31`.
fn require_module_file(
    module_file: &Path,
    deps_path: &Path,
    line: u32,
    name: &str,
    package: Option<&str>,
) -> Result<Option<PathBuf>, Located> {
    if package.is_some() || module_file.is_file() {
        // A package's file is not checked here: the loader reads it next and
        // names the import that asked for a file the dependency lacks, which
        // is a better place to point than the `deps` line.
        Ok(Some(module_file.to_path_buf()))
    } else {
        Err(located(
            deps_path,
            line,
            format!(
                "remote import `{name}` has no `{name}.m31` at its repository's \
                 root: a remote import is exactly one file, named after the \
                 import, at the top of the repo"
            ),
        ))
    }
}

/// Does `dir`'s `deps` manifest name `name`? The loader asks before it
/// resolves anything, because a dependency and a local directory of the same
/// name are an ambiguity to refuse, and refusing it after a `git clone` is
/// the wrong order.
pub fn declares(dir: &Path, name: &str) -> Result<bool, Located> {
    Ok(read_manifest(dir)?.is_some_and(|m| m.deps.contains_key(name)))
}

/// A file found inside a dependency, and the root that file's own imports
/// resolve against: the dependency's directory.
pub struct Package {
    pub file: PathBuf,
    pub root: PathBuf,
}

/// Try to resolve `name` as a one-file remote import: the fallback `deps.rs`
/// adds to `Loader::visit`, consulted only after the embedded standard
/// library and the plain `name.m31`-beside-the-entry-file check have both
/// already missed.
///
/// `Ok(None)` means this module has nothing to say -- no `deps` file beside
/// `dir`, or one that exists but never mentions `name` -- and the caller
/// falls through to its own "cannot find module" error exactly as it did
/// before this fallback existed. `Ok(Some(path))` is a file on disk, ready to
/// be read and parsed exactly like a local import. Anything else is a
/// `Located` diagnostic: a malformed manifest line, a `git` that could not
/// be run, a clone or checkout that failed, or a lock entry the cache could
/// not be made to match.
pub fn resolve(dir: &Path, name: &str) -> Result<Option<PathBuf>, Located> {
    Ok(resolve_in(dir, name, None)?.map(|p| p.file))
}

/// A file inside a dependency that is a whole directory: `import tui.geom;`
/// asks `dep` = `tui` for `rel` = `geom.m31`. A git dependency is fetched,
/// locked and cached exactly as a one-file one is; a path dependency is just
/// the directory it names. `Ok(None)` only when `deps` does not name `dep`.
pub fn resolve_package(dir: &Path, dep: &str, rel: &str) -> Result<Option<Package>, Located> {
    resolve_in(dir, dep, Some(rel))
}

/// A `path` dependency, used in place. It is a project in its own right, so
/// it must carry its own `deps` manifest; its own dependencies are not
/// followed (the v0 rule that holds for git dependencies too).
fn resolve_path_dep(
    dir: &Path,
    deps_path: &Path,
    line: u32,
    name: &str,
    rel_dir: &str,
    package: Option<&str>,
) -> Result<Package, Located> {
    let dep_dir = if Path::new(rel_dir).is_absolute() {
        PathBuf::from(rel_dir)
    } else {
        clean(&dir.join(rel_dir))
    };
    if !dep_dir.is_dir() {
        return Err(located(
            deps_path,
            line,
            format!(
                "path dependency `{name}` points at `{rel_dir}`, which is not a \
                 directory (looked for {})",
                dep_dir.display()
            ),
        ));
    }
    if read_manifest(&dep_dir)?.is_none() {
        return Err(located(
            deps_path,
            line,
            format!(
                "path dependency `{name}` ({rel_dir}) has no `deps` file: a \
                 dependency is a project, so add one there with its `name` and \
                 `version` lines"
            ),
        ));
    }
    let file = match package {
        Some(rel) => dep_dir.join(rel),
        None => {
            let f = dep_dir.join(format!("{name}.m31"));
            if !f.is_file() {
                return Err(located(
                    deps_path,
                    line,
                    format!(
                        "path dependency `{name}` has no `{name}.m31` at its \
                         root: importing `{name}` alone is exactly one file, \
                         named after the import, at the top of the directory"
                    ),
                ));
            }
            f
        }
    };
    Ok(Package {
        file,
        root: dep_dir,
    })
}

fn resolve_in(dir: &Path, name: &str, package: Option<&str>) -> Result<Option<Package>, Located> {
    let deps_path = dir.join("deps");
    let Some(manifest) = read_manifest(dir)? else {
        return Ok(None);
    };
    let Some(dep) = manifest.deps.get(name) else {
        return Ok(None);
    };
    let dep_line = dep.line;
    let (url, git_ref) = match &dep.source {
        Source::Path(rel_dir) => {
            return resolve_path_dep(dir, &deps_path, dep_line, name, rel_dir, package).map(Some);
        }
        Source::Git { url, git_ref } => (resolve_url(dir, url), git_ref.clone()),
    };

    let lock_path = dir.join("deps.lock");
    let lock_text = std::fs::read_to_string(&lock_path).unwrap_or_default();
    let locks = parse_manifest(&lock_path, &lock_text, 1, "deps.lock")?;

    let cache_dir = dir.join(".m31-deps").join(name);
    let module_file = match package {
        Some(rel) => cache_dir.join(rel),
        None => cache_dir.join(format!("{name}.m31")),
    };
    let found = |file: PathBuf| Package {
        file,
        root: cache_dir.clone(),
    };

    if let Some(lock) = locks.get(name) {
        // `deps.lock` is authoritative once a line exists: `ref` is never
        // consulted again, not even to check it still points somewhere
        // sensible. See the module doc comment and
        // docs/remote-imports-decision.md §2.
        let commit = lock.fields[0].clone();
        if !cache_matches(&cache_dir, &commit) {
            reclone(&url, &cache_dir, &deps_path, dep_line, name)?;
            checkout_locked_commit(&cache_dir, &commit, &lock_path, lock.line, name)?;
        }
        // A cache that already matches needs no network access, per the
        // decision doc's resolution flow.
        return require_module_file(&module_file, &deps_path, dep_line, name, package)
            .map(|f| f.map(found));
    }

    // No lock entry yet: this is the first time anything has asked for
    // `name`. Clone, check out `ref`, and whatever commit that lands on
    // becomes the lock -- permanently, until a human deletes the line.
    reclone(&url, &cache_dir, &deps_path, dep_line, name)?;
    checkout_ref(&cache_dir, &git_ref, &deps_path, dep_line, name)?;
    let commit = rev_parse_head(&cache_dir, &deps_path, dep_line, name)?;
    append_lock(&lock_path, name, &commit).map_err(|e| {
        located(
            &deps_path,
            dep_line,
            format!("cannot write {}: {e}", lock_path.display()),
        )
    })?;
    require_module_file(&module_file, &deps_path, dep_line, name, package).map(|f| f.map(found))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, MutexGuard};

    /// `cargo test` runs this module's tests in parallel threads of one
    /// process, but one of them (`git_not_on_path_...`) has to mutate the
    /// process-wide `PATH` environment variable, which every other test here
    /// also depends on to find the real `git`. A lock held for a whole
    /// test's body is what keeps that mutation from racing a concurrent
    /// test's own git invocation -- `.lock().unwrap_or_else(PoisonError::
    /// into_inner)` because one test panicking (an assertion failure) must
    /// not poison the lock for every test after it.
    static GIT_SERIAL: Mutex<()> = Mutex::new(());
    fn serial() -> MutexGuard<'static, ()> {
        GIT_SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A fresh scratch directory under the system temp dir, unique per call
    /// so tests running in parallel (the default for `cargo test`) cannot
    /// collide -- the same reasoning corpus/modules/stdlib-fs's own fixture
    /// comment gives for naming its tree from a random source, just done
    /// here with a process-local counter plus the PID instead, since this is
    /// Rust rather than the language under test and has no `random` module
    /// to reach for.
    fn scratch_dir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "m31-deps-test-{}-{}-{}",
            std::process::id(),
            n,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn git_ok(args: &[&str], cwd: &Path) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git must be on PATH to run these tests");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Build a throwaway local git repository standing in for "the remote":
    /// `git init`, commit one `<name>.m31` file with the given contents,
    /// tag it `v1`. No network involved anywhere -- `git clone` works on a
    /// plain local path exactly as it would on a URL, which is what makes
    /// this hermetic.
    fn make_remote(name: &str, contents: &str) -> PathBuf {
        let remote = scratch_dir();
        git_ok(&["init", "--quiet", "-b", "main"], &remote);
        git_ok(&["config", "user.email", "test@example.com"], &remote);
        git_ok(&["config", "user.name", "test"], &remote);
        std::fs::write(remote.join(format!("{name}.m31")), contents).unwrap();
        git_ok(&["add", "."], &remote);
        git_ok(&["commit", "--quiet", "-m", "initial"], &remote);
        git_ok(&["tag", "v1"], &remote);
        remote
    }

    const HEADER: &str = "name test\nversion 0.1.0\n";

    fn write_deps(project: &Path, name: &str, url: &Path, git_ref: &str) {
        std::fs::write(
            project.join("deps"),
            format!("{HEADER}{name} {} {git_ref}\n", url.display()),
        )
        .unwrap();
    }

    #[test]
    fn no_deps_file_resolves_to_nothing() {
        let _guard = serial();
        let project = scratch_dir();
        assert!(resolve(&project, "whatever").unwrap().is_none());
    }

    #[test]
    fn deps_file_without_this_name_resolves_to_nothing() {
        let _guard = serial();
        let project = scratch_dir();
        let remote = make_remote("greet", "pub str hi() { return \"hi\"; }\n");
        write_deps(&project, "other", &remote, "v1");
        assert!(resolve(&project, "greet").unwrap().is_none());
    }

    #[test]
    fn first_resolution_clones_checks_out_and_locks() {
        let _guard = serial();
        let project = scratch_dir();
        let remote = make_remote("greet", "pub str hi() { return \"hi\"; }\n");
        write_deps(&project, "greet", &remote, "v1");

        let resolved = resolve(&project, "greet")
            .expect("resolution should succeed")
            .expect("greet is named in deps");
        assert_eq!(resolved, project.join(".m31-deps/greet/greet.m31"));
        assert_eq!(
            std::fs::read_to_string(&resolved).unwrap(),
            "pub str hi() { return \"hi\"; }\n"
        );

        // The lock was written, naming the commit `v1` actually resolved to.
        let want_commit = {
            let out = std::process::Command::new("git")
                .args(["rev-parse", "v1"])
                .current_dir(&remote)
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let lock = std::fs::read_to_string(project.join("deps.lock")).unwrap();
        assert_eq!(lock, format!("greet {want_commit}\n"));
    }

    #[test]
    fn second_resolution_reuses_cache_with_no_clone() {
        let _guard = serial();
        let project = scratch_dir();
        let remote = make_remote("greet", "pub str hi() { return \"hi\"; }\n");
        write_deps(&project, "greet", &remote, "v1");
        resolve(&project, "greet").unwrap().unwrap();

        // Prove the second resolution never shells out to `git clone` again:
        // move the "remote" out of the way entirely. A second `resolve`
        // that still succeeds, unchanged, could only have used the existing
        // cache -- a clone or checkout against the now-missing path would
        // fail outright, since there is nothing left to clone from.
        let moved = remote.with_extension("moved");
        std::fs::rename(&remote, &moved).unwrap();

        let resolved = resolve(&project, "greet")
            .expect("cache hit must not need the remote at all")
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&resolved).unwrap(),
            "pub str hi() { return \"hi\"; }\n"
        );
        // The lock line is untouched -- appended once, never rewritten.
        let lock = std::fs::read_to_string(project.join("deps.lock")).unwrap();
        assert_eq!(lock.lines().count(), 1);
    }

    #[test]
    fn stale_cache_is_reset_to_the_locked_commit_not_ref() {
        let _guard = serial();
        let project = scratch_dir();
        let remote = make_remote("greet", "pub str hi() { return \"one\"; }\n");
        write_deps(&project, "greet", &remote, "v1");
        resolve(&project, "greet").unwrap().unwrap();

        // Move `v1` on the remote, the way an upstream branch would drift.
        std::fs::write(
            remote.join("greet.m31"),
            "pub str hi() { return \"two\"; }\n",
        )
        .unwrap();
        git_ok(&["add", "."], &remote);
        git_ok(&["commit", "--quiet", "-m", "second"], &remote);
        git_ok(&["tag", "-f", "v1"], &remote);

        // Corrupt the cache so it has to be reset.
        std::fs::remove_dir_all(project.join(".m31-deps/greet")).unwrap();

        let resolved = resolve(&project, "greet").unwrap().unwrap();
        // The LOCKED commit wins, not the now-moved `v1` -- still "one".
        assert_eq!(
            std::fs::read_to_string(&resolved).unwrap(),
            "pub str hi() { return \"one\"; }\n"
        );
    }

    #[test]
    fn malformed_deps_line_is_a_located_diagnostic() {
        let _guard = serial();
        let project = scratch_dir();
        std::fs::write(
            project.join("deps"),
            format!("{HEADER}greet only-two-fields\n"),
        )
        .unwrap();
        let err = resolve(&project, "greet").unwrap_err();
        assert!(err.path.ends_with("deps"));
        assert_eq!(err.diag.span, Span::new(3, 1));
        assert!(err.diag.msg.contains("malformed `deps` line"));
    }

    #[test]
    fn nonexistent_ref_is_a_clear_error_not_a_panic() {
        let _guard = serial();
        let project = scratch_dir();
        let remote = make_remote("greet", "pub str hi() { return \"hi\"; }\n");
        write_deps(&project, "greet", &remote, "no-such-ref");
        let err = resolve(&project, "greet").unwrap_err();
        assert!(err.path.ends_with("deps"));
        assert!(err.diag.msg.contains("cannot check out `no-such-ref`"));
    }

    #[test]
    fn lock_naming_an_unreachable_commit_is_a_clear_error() {
        let _guard = serial();
        let project = scratch_dir();
        let remote = make_remote("greet", "pub str hi() { return \"hi\"; }\n");
        write_deps(&project, "greet", &remote, "v1");
        // A lock entry naming a commit that was never part of this history
        // -- the "remote history was rewritten" case from the decision doc,
        // reproduced without actually rewriting anything.
        std::fs::write(
            project.join("deps.lock"),
            "greet 0000000000000000000000000000000000000000\n",
        )
        .unwrap();
        let err = resolve(&project, "greet").unwrap_err();
        assert!(err.path.ends_with("deps.lock"));
        assert!(err.diag.msg.contains("deps.lock` names commit"));
    }

    #[test]
    fn git_not_on_path_is_a_clear_error_not_a_panic() {
        let _guard = serial();
        let project = scratch_dir();
        let remote = make_remote("greet", "pub str hi() { return \"hi\"; }\n");
        write_deps(&project, "greet", &remote, "v1");

        // Run the resolution with a PATH that has no `git` on it at all --
        // a directory containing only a `cargo` link, say, never `git`
        // itself. `std::process::Command` looks `git` up through `PATH` at
        // spawn time, so clearing it here reaches the exact failure mode
        // without needing a fake binary.
        let old_path = std::env::var_os("PATH");
        std::env::set_var("PATH", "");
        let result = resolve(&project, "greet");
        if let Some(p) = old_path {
            std::env::set_var("PATH", p);
        }
        let err = result.unwrap_err();
        assert!(err.diag.msg.contains("`git` was not found on PATH"));
    }

    #[test]
    fn relative_url_resolves_against_the_deps_directory() {
        let _guard = serial();
        // The project directory and the "remote" both sit under one parent,
        // so the `deps` file can name the remote with a path relative to
        // itself -- exactly what a committed corpus fixture needs, since it
        // cannot know where the repository will be checked out.
        let parent = scratch_dir();
        let project = parent.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let remote = parent.join("fixture.git");
        std::fs::create_dir_all(&remote).unwrap();
        git_ok(&["init", "--quiet", "-b", "main"], &remote);
        git_ok(&["config", "user.email", "test@example.com"], &remote);
        git_ok(&["config", "user.name", "test"], &remote);
        std::fs::write(
            remote.join("greet.m31"),
            "pub str hi() { return \"hi\"; }\n",
        )
        .unwrap();
        git_ok(&["add", "."], &remote);
        git_ok(&["commit", "--quiet", "-m", "initial"], &remote);
        git_ok(&["tag", "v1"], &remote);

        std::fs::write(
            project.join("deps"),
            format!("{HEADER}greet ../fixture.git v1\n"),
        )
        .unwrap();
        let resolved = resolve(&project, "greet").unwrap().unwrap();
        assert_eq!(
            std::fs::read_to_string(&resolved).unwrap(),
            "pub str hi() { return \"hi\"; }\n"
        );
    }

    fn manifest_err(text: &str) -> Located {
        let project = scratch_dir();
        std::fs::write(project.join("deps"), text).unwrap();
        match read_manifest(&project) {
            Err(e) => e,
            Ok(_) => panic!("{text:?} should be refused"),
        }
    }

    #[test]
    fn a_manifest_needs_a_name_and_a_version() {
        assert!(manifest_err("").diag.msg.contains("no project header"));
        assert!(manifest_err("# only a comment\n")
            .diag
            .msg
            .contains("no project header"));
        assert!(manifest_err("name app\n")
            .diag
            .msg
            .contains("missing its `version` line"));
        assert!(manifest_err("version 0.1.0\n")
            .diag
            .msg
            .contains("missing its `name` line"));
    }

    #[test]
    fn name_and_version_are_validated() {
        assert!(manifest_err("name a/b\nversion 0.1.0\n")
            .diag
            .msg
            .contains("is not a project name"));
        for bad in ["1", "1.0", "1.0.x", "1..0", "v1.0.0", "1.0.0-be!ta"] {
            let e = manifest_err(&format!("name a\nversion {bad}\n"));
            assert!(e.diag.msg.contains("is not a version"), "{bad}");
        }
        let project = scratch_dir();
        for good in [
            "0.1.0",
            "10.20.30",
            "1.0.0-rc.1",
            "1.0.0+build.5",
            "1.0.0-a+b",
        ] {
            std::fs::write(
                project.join("deps"),
                format!("name my-app_2\nversion {good}\n"),
            )
            .unwrap();
            assert!(read_manifest(&project).is_ok(), "{good}");
        }
    }

    #[test]
    fn name_and_version_cannot_be_dependencies_and_not_repeated() {
        let e = manifest_err("name app\nversion 1.0.0\nname x/y/z url ref\n");
        assert!(e.diag.msg.contains("malformed `name` line"));
        assert_eq!(e.diag.span, Span::new(3, 1));
        let e = manifest_err("name a\nversion 1.0.0\nversion 2.0.0\n");
        assert!(e.diag.msg.contains("duplicate `version`"));
        let e = manifest_err("name a\nversion 1.0.0\nd path x\nd path y\n");
        assert!(e.diag.msg.contains("duplicate dependency `d`"));
    }

    #[test]
    fn a_path_dependency_resolves_in_place_with_no_lock() {
        let _guard = serial();
        let parent = scratch_dir();
        let project = parent.join("app");
        let dep = parent.join("libs/tui");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&dep).unwrap();
        std::fs::write(dep.join("deps"), "name tui\nversion 0.1.0\n").unwrap();
        std::fs::write(dep.join("geom.m31"), "pub int one() { return 1; }\n").unwrap();
        std::fs::write(
            project.join("deps"),
            format!("{HEADER}tui path ../libs/tui\n"),
        )
        .unwrap();

        let pkg = resolve_package(&project, "tui", "geom.m31")
            .unwrap()
            .unwrap();
        // `..` is folded away, so diagnostics print the tidy path.
        assert_eq!(pkg.root, parent.join("libs/tui"));
        assert_eq!(pkg.file, parent.join("libs/tui/geom.m31"));
        // Nothing fetched, nothing locked, nothing cached.
        assert!(!project.join("deps.lock").exists());
        assert!(!project.join(".m31-deps").exists());
    }

    #[test]
    fn a_path_dependency_must_exist_and_be_a_project() {
        let _guard = serial();
        let parent = scratch_dir();
        std::fs::write(parent.join("deps"), format!("{HEADER}tui path libs/tui\n")).unwrap();
        let e = resolve_package(&parent, "tui", "geom.m31").err().unwrap();
        assert!(e.diag.msg.contains("is not a directory"));
        std::fs::create_dir_all(parent.join("libs/tui")).unwrap();
        let e = resolve_package(&parent, "tui", "geom.m31").err().unwrap();
        assert!(e.diag.msg.contains("has no `deps` file"));
        // And its own header is validated, reported against ITS file.
        std::fs::write(parent.join("libs/tui/deps"), "name tui\n").unwrap();
        let e = resolve_package(&parent, "tui", "geom.m31").err().unwrap();
        assert!(e.path.ends_with("libs/tui/deps"));
    }

    #[test]
    fn clean_folds_dots_lexically() {
        assert_eq!(clean(Path::new("a/b/../c/./d")), PathBuf::from("a/c/d"));
        assert_eq!(clean(Path::new("a/../..")), PathBuf::from(".."));
        assert_eq!(clean(Path::new("a/..")), PathBuf::from("."));
    }
}
