//! Transitive dependencies: a dependency's own `deps` is followed. Chains,
//! diamonds, cycles, conflicts, and the git side (lock and cache are the top
//! project's, flat by name).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::resolve::modules::load;

fn scratch() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "m31-transitive-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &Path, rel: &str, text: &str) -> String {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, text).unwrap();
    p.to_string_lossy().into_owned()
}

fn header(name: &str) -> String {
    format!("name {name}\nversion 0.1.0\n")
}

/// `<dir>/deps` for project `name` with the given dependency lines.
fn project(dir: &Path, rel: &str, name: &str, lines: &[&str]) {
    let mut text = header(name);
    for l in lines {
        text.push_str(l);
        text.push('\n');
    }
    write(dir, &format!("{rel}/deps"), &text);
}

fn module(dir: &Path, rel: &str, imports: &[&str], func: &str, body: &str) {
    let mut text = String::new();
    for i in imports {
        text.push_str(&format!("import {i};\n"));
    }
    text.push_str(&format!("pub int {func}() {{\n    return {body};\n}}\n"));
    write(dir, rel, &text);
}

fn err_of(entry: &str) -> String {
    match load(entry) {
        Ok(_) => panic!("expected the load to be refused"),
        Err(e) => e.diag.msg,
    }
}

#[test]
fn a_dependency_of_a_dependency_of_a_dependency_is_found() {
    let d = scratch();
    project(&d, "app", "app", &["mid path ../mid"]);
    project(&d, "mid", "mid", &["leaf path ../leaf"]);
    project(&d, "leaf", "leaf", &["base path ../base"]);
    project(&d, "base", "base", &[]);
    module(&d, "base/basenum.m31", &[], "base", "100");
    module(
        &d,
        "leaf/leafcore.m31",
        &["base.basenum"],
        "up",
        "basenum.base() + 1",
    );
    module(
        &d,
        "mid/midapi.m31",
        &["leaf.leafcore"],
        "top",
        "leafcore.up() + 1",
    );
    let t = write(
        &d,
        "app/main.m31",
        "import mid.midapi;\nprint(str(midapi.top()));\n",
    );
    let loaded = load(&t).unwrap_or_else(|e| panic!("{}", e.diag.msg));
    for m in ["midapi", "leafcore", "basenum"] {
        assert!(loaded.paths.contains_key(m), "{m}");
    }
}

#[test]
fn paths_in_a_dependencys_deps_are_relative_to_that_dependency() {
    let d = scratch();
    project(&d, "app", "app", &["mid path libs/mid"]);
    // `../leaf` from `app/libs/mid` is `app/libs/leaf`, not `app/../leaf`.
    project(&d, "app/libs/mid", "mid", &["leaf path ../leaf"]);
    project(&d, "app/libs/leaf", "leaf", &[]);
    module(&d, "app/libs/leaf/leafcore.m31", &[], "one", "1");
    module(
        &d,
        "app/libs/mid/midapi.m31",
        &["leaf.leafcore"],
        "f",
        "leafcore.one()",
    );
    let t = write(
        &d,
        "app/main.m31",
        "import mid.midapi;\nprint(str(midapi.f()));\n",
    );
    assert!(load(&t).is_ok());
}

#[test]
fn a_diamond_is_resolved_once_even_when_spelt_differently() {
    let d = scratch();
    project(&d, "app", "app", &["a path ../a", "b path ../b"]);
    project(&d, "a", "a", &["util path ../util"]);
    // The same directory by another route.
    project(&d, "b", "b", &["util path ../b/../util"]);
    project(&d, "util", "util", &[]);
    module(&d, "util/util.m31", &[], "one", "1");
    module(&d, "a/amod.m31", &["util"], "f", "util.one()");
    module(&d, "b/bmod.m31", &["util"], "g", "util.one()");
    let t = write(
        &d,
        "app/main.m31",
        "import a.amod;\nimport b.bmod;\nprint(str(amod.f() + bmod.g()));\n",
    );
    let loaded = load(&t).unwrap_or_else(|e| panic!("{}", e.diag.msg));
    assert!(loaded.paths.contains_key("util"));
}

#[test]
fn a_project_may_also_declare_what_a_dependency_declares() {
    let d = scratch();
    project(&d, "app", "app", &["a path ../a", "util path ../util"]);
    project(&d, "a", "a", &["util path ../util"]);
    project(&d, "util", "util", &[]);
    module(&d, "util/util.m31", &[], "one", "1");
    module(&d, "a/amod.m31", &["util"], "f", "util.one()");
    let t = write(
        &d,
        "app/main.m31",
        "import a.amod;\nimport util;\nprint(str(amod.f() + util.one()));\n",
    );
    assert!(load(&t).is_ok());
}

#[test]
fn a_dependency_cannot_see_its_consumers_dependencies() {
    let d = scratch();
    project(&d, "app", "app", &["a path ../a", "util path ../util"]);
    project(&d, "a", "a", &[]);
    project(&d, "util", "util", &[]);
    module(&d, "util/util.m31", &[], "one", "1");
    // `a` did not list `util`, so it is not `a`'s to import.
    module(&d, "a/amod.m31", &["util"], "f", "util.one()");
    let t = write(
        &d,
        "app/main.m31",
        "import a.amod;\nprint(str(amod.f()));\n",
    );
    assert!(err_of(&t).contains("cannot find module `util`"));
}

#[test]
fn a_cycle_names_the_whole_chain() {
    let d = scratch();
    project(&d, "app", "app", &["a path ../a"]);
    project(&d, "a", "a", &["b path ../b"]);
    project(&d, "b", "b", &["c path ../c"]);
    project(&d, "c", "c", &["a path ../a"]);
    let t = write(&d, "app/main.m31", "print(\"hi\");\n");
    let msg = err_of(&t);
    assert!(
        msg.contains("dependency cycle: app -> a -> b -> c -> a"),
        "{msg}"
    );
}

#[test]
fn a_dependency_that_depends_on_the_project_is_a_cycle() {
    let d = scratch();
    project(&d, "app", "app", &["a path ../a"]);
    project(&d, "a", "a", &["app path ../app"]);
    let t = write(&d, "app/main.m31", "print(\"hi\");\n");
    let msg = err_of(&t);
    assert!(msg.contains("dependency cycle: app -> a -> app"), "{msg}");
}

#[test]
fn a_conflict_names_both_chains_and_sources() {
    let d = scratch();
    project(&d, "app", "app", &["a path ../a", "b path ../b"]);
    project(&d, "a", "a", &["util path ../util1"]);
    project(&d, "b", "b", &["util path ../util2"]);
    project(&d, "util1", "util", &[]);
    project(&d, "util2", "util", &[]);
    let t = write(&d, "app/main.m31", "print(\"hi\");\n");
    let msg = err_of(&t);
    assert!(msg.contains("dependency `util` has two different sources"));
    assert!(
        msg.contains("`path ../util1` via app -> a -> util"),
        "{msg}"
    );
    assert!(
        msg.contains("`path ../util2` via app -> b -> util"),
        "{msg}"
    );
}

#[test]
fn an_unimported_branch_still_conflicts() {
    // Found without importing anything: the path graph is walked up front.
    let d = scratch();
    project(&d, "app", "app", &["util path ../util1", "a path ../a"]);
    project(&d, "a", "a", &["util path ../util2"]);
    project(&d, "util1", "util", &[]);
    project(&d, "util2", "util", &[]);
    let t = write(&d, "app/main.m31", "print(\"hi\");\n");
    let msg = err_of(&t);
    assert!(msg.contains("via app -> util"), "{msg}");
    assert!(msg.contains("via app -> a -> util"), "{msg}");
}

fn git_ok(args: &[&str], cwd: &Path) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git must be on PATH to run these tests");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A local repository holding `files`, committed and tagged `v1`.
fn remote(dir: &Path, rel: &str, files: &[(&str, &str)]) -> PathBuf {
    let r = dir.join(rel);
    std::fs::create_dir_all(&r).unwrap();
    git_ok(&["init", "--quiet", "-b", "main"], &r);
    git_ok(&["config", "user.email", "test@example.com"], &r);
    git_ok(&["config", "user.name", "test"], &r);
    for (name, text) in files {
        write(&r, name, text);
    }
    git_ok(&["add", "."], &r);
    git_ok(&["commit", "--quiet", "-m", "initial"], &r);
    git_ok(&["tag", "v1"], &r);
    r
}

#[test]
fn a_git_dependencys_own_deps_are_fetched_and_locked_at_the_top() {
    let _guard = crate::resolve::deps::git_serial();
    let d = scratch();
    let base = remote(
        &d,
        "remotes/base",
        &[
            ("deps", &header("base")),
            ("basenum.m31", "pub int base() {\n    return 100;\n}\n"),
        ],
    );
    let lib_deps = format!("{}b {} v1\n", header("lib"), base.display());
    let lib = remote(
        &d,
        "remotes/lib",
        &[
            ("deps", &lib_deps),
            (
                "libmod.m31",
                "import b.basenum;\npub int up() {\n    return basenum.base() + 1;\n}\n",
            ),
        ],
    );
    write(
        &d,
        "app/deps",
        &format!("{}lib {} v1\n", header("app"), lib.display()),
    );
    let t = write(
        &d,
        "app/main.m31",
        "import lib.libmod;\nprint(str(libmod.up()));\n",
    );
    let loaded = load(&t).unwrap_or_else(|e| panic!("{}", e.diag.msg));
    assert!(loaded.paths.contains_key("basenum"));
    // Both are locked in the top project's lock and cached beside it.
    let lock = std::fs::read_to_string(d.join("app/deps.lock")).unwrap();
    assert_eq!(lock.lines().count(), 2, "{lock}");
    assert!(lock.contains("lib ") && lock.contains("b "), "{lock}");
    assert!(d.join("app/.m31-deps/b/basenum.m31").is_file());
    assert!(!d.join("app/.m31-deps/lib/.m31-deps").exists());
    assert!(!d.join("app/.m31-deps/lib/deps.lock").exists());

    // Second build, remotes gone: lock and cache are enough.
    std::fs::rename(d.join("remotes"), d.join("gone")).unwrap();
    assert!(load(&t).is_ok());
}

#[test]
fn a_git_dependency_may_use_a_path_dependency_inside_its_checkout() {
    let _guard = crate::resolve::deps::git_serial();
    let d = scratch();
    let lib = remote(
        &d,
        "remotes/lib",
        &[
            ("deps", &format!("{}innerlib path inner\n", header("lib"))),
            ("inner/deps", &header("inner")),
            ("inner/innermod.m31", "pub int one() {\n    return 1;\n}\n"),
            (
                "libmod.m31",
                "import innerlib.innermod;\npub int f() {\n    return innermod.one();\n}\n",
            ),
        ],
    );
    write(
        &d,
        "app/deps",
        &format!("{}lib {} v1\n", header("app"), lib.display()),
    );
    let t = write(
        &d,
        "app/main.m31",
        "import lib.libmod;\nprint(str(libmod.f()));\n",
    );
    let loaded = load(&t).unwrap_or_else(|e| panic!("{}", e.diag.msg));
    assert!(loaded.paths.contains_key("innermod"));
}

#[test]
fn a_one_file_git_dependency_reaches_its_own_dependencies() {
    let _guard = crate::resolve::deps::git_serial();
    let d = scratch();
    let util = remote(
        &d,
        "remotes/util",
        &[("util.m31", "pub int one() {\n    return 1;\n}\n")],
    );
    let one = remote(
        &d,
        "remotes/one",
        &[
            (
                "deps",
                &format!("{}util {} v1\n", header("one"), util.display()),
            ),
            (
                "one.m31",
                "import util;\npub int f() {\n    return util.one() + 1;\n}\n",
            ),
        ],
    );
    write(
        &d,
        "app/deps",
        &format!("{}one {} v1\n", header("app"), one.display()),
    );
    let t = write(&d, "app/main.m31", "import one;\nprint(str(one.f()));\n");
    let loaded = load(&t).unwrap_or_else(|e| panic!("{}", e.diag.msg));
    assert!(loaded.paths.contains_key("util"));
}

#[test]
fn a_git_conflict_with_a_path_dependency_is_refused() {
    let d = scratch();
    let util = remote(
        &d,
        "remotes/util",
        &[("util.m31", "pub int one() {\n    return 1;\n}\n")],
    );
    let lib = remote(
        &d,
        "remotes/lib",
        &[
            (
                "deps",
                &format!("{}util {} v1\n", header("lib"), util.display()),
            ),
            ("lib.m31", "pub int f() {\n    return 1;\n}\n"),
        ],
    );
    project(&d, "app", "app", &["util path ../local-util"]);
    let mut text = std::fs::read_to_string(d.join("app/deps")).unwrap();
    text.push_str(&format!("lib {} v1\n", lib.display()));
    write(&d, "app/deps", &text);
    project(&d, "local-util", "util", &[]);
    module(&d, "local-util/util.m31", &[], "one", "1");
    let t = write(&d, "app/main.m31", "import lib;\nprint(str(lib.f()));\n");
    let msg = err_of(&t);
    assert!(
        msg.contains("dependency `util` has two different sources"),
        "{msg}"
    );
    assert!(msg.contains("via app -> util"), "{msg}");
    assert!(msg.contains("via app -> lib -> util"), "{msg}");
}
