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
//! - `deps` -- intent: `name url ref`, one remote import per line.
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
fn require_module_file(
    module_file: &Path,
    deps_path: &Path,
    line: u32,
    name: &str,
) -> Result<Option<PathBuf>, Located> {
    if module_file.is_file() {
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

/// Try to resolve `name` as a remote import: the one new fallback `deps.rs`
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
    let deps_path = dir.join("deps");
    let Ok(deps_text) = std::fs::read_to_string(&deps_path) else {
        return Ok(None);
    };
    let deps = parse_manifest(&deps_path, &deps_text, 2, "deps")?;
    let Some(dep) = deps.get(name) else {
        return Ok(None);
    };
    let url = resolve_url(dir, &dep.fields[0]);
    let git_ref = dep.fields[1].clone();
    let dep_line = dep.line;

    let lock_path = dir.join("deps.lock");
    let lock_text = std::fs::read_to_string(&lock_path).unwrap_or_default();
    let locks = parse_manifest(&lock_path, &lock_text, 1, "deps.lock")?;

    let cache_dir = dir.join(".m31-deps").join(name);
    let module_file = cache_dir.join(format!("{name}.m31"));

    if let Some(lock) = locks.get(name) {
        // `deps.lock` is authoritative once a line exists: `ref` is never
        // consulted again, not even to check it still points somewhere
        // sensible. See the module doc comment and
        // docs/remote-imports-decision.md §2.
        let commit = lock.fields[0].clone();
        if cache_matches(&cache_dir, &commit) {
            // Already checked out, already matches -- no network access,
            // per the decision doc's resolution flow.
            return require_module_file(&module_file, &deps_path, dep_line, name);
        }
        reclone(&url, &cache_dir, &deps_path, dep_line, name)?;
        checkout_locked_commit(&cache_dir, &commit, &lock_path, lock.line, name)?;
        return require_module_file(&module_file, &deps_path, dep_line, name);
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
    require_module_file(&module_file, &deps_path, dep_line, name)
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

    fn write_deps(project: &Path, name: &str, url: &Path, git_ref: &str) {
        std::fs::write(
            project.join("deps"),
            format!("{name} {} {git_ref}\n", url.display()),
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
        std::fs::write(project.join("deps"), "greet only-two-fields\n").unwrap();
        let err = resolve(&project, "greet").unwrap_err();
        assert!(err.path.ends_with("deps"));
        assert_eq!(err.diag.span, Span::new(1, 1));
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

        std::fs::write(project.join("deps"), "greet ../fixture.git v1\n").unwrap();
        let resolved = resolve(&project, "greet").unwrap().unwrap();
        assert_eq!(
            std::fs::read_to_string(&resolved).unwrap(),
            "pub str hi() { return \"hi\"; }\n"
        );
    }
}
