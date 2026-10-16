//! `m31c init`: the layout it plans, its refusals, and that what it writes
//! passes the project's own fmt and lint checks.

use std::path::PathBuf;
use std::process::ExitCode;

use crate::tools::init::{self, Opts};
use crate::tools::lint;

fn opts(name: &str, prefix: &str, lib: bool) -> Opts {
    Opts {
        name: name.into(),
        prefix: prefix.into(),
        lib,
    }
}

fn paths(o: &Opts) -> Vec<String> {
    init::plan(o)
        .expect("expected a plan")
        .into_iter()
        .map(|f| f.path)
        .collect()
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("m31-init-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn app_layout() {
    let p = paths(&opts("my-app", "MA", false));
    for want in [
        "deps",
        "README.md",
        "RENAMES.md",
        "my_app.m31",
        "core/MA_core.m31",
        "tests/t_core.m31",
        "tests/test.sh",
        "scripts/build.sh",
        "docs/design.md",
        ".github/workflows/test.yml",
        ".github/workflows/release.yml",
    ] {
        assert!(p.iter().any(|x| x == want), "missing {want}: {p:?}");
    }
}

#[test]
fn lib_has_no_entry_point_or_build_script() {
    let p = paths(&opts("my-lib", "ML", true));
    assert!(!p
        .iter()
        .any(|x| x == "my_lib.m31" || x == "scripts/build.sh"));
    assert!(p.iter().any(|x| x == "core/ML_core.m31"));
}

#[test]
fn default_prefixes() {
    assert_eq!(init::default_prefix("tui"), "TUI");
    assert_eq!(init::default_prefix("my-cool-app"), "MCA");
}

#[test]
fn bad_names_and_prefixes_are_refused() {
    for name in ["", "My", "-a", "a-", "a--b", "1a", "a_b", "os"] {
        assert!(init::plan(&opts(name, "A", false)).is_err(), "{name}");
    }
    for prefix in ["", "a", "A-B", "1A", "Ab"] {
        assert!(init::plan(&opts("app", prefix, false)).is_err(), "{prefix}");
    }
}

#[test]
fn no_placeholder_survives() {
    for lib in [false, true] {
        for f in init::plan(&opts("my-app", "MA", lib)).unwrap() {
            assert!(!f.text.contains("@@"), "{} has a placeholder", f.path);
        }
    }
}

#[test]
fn nothing_is_overwritten() {
    let dir = scratch("exists");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("README.md"), "mine").unwrap();
    let files = init::plan(&opts("app", "APP", false)).unwrap();
    let e = init::write_all(&dir, &files).expect_err("expected a refusal");
    assert!(e.contains("README.md"), "{e}");
    assert_eq!(
        std::fs::read_to_string(dir.join("README.md")).unwrap(),
        "mine"
    );
    assert!(!dir.join("deps").exists(), "wrote despite the refusal");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn written_project_is_formatted_and_lint_clean() {
    for lib in [false, true] {
        let dir = scratch(if lib { "lib" } else { "app" });
        let files = init::plan(&opts("my-app", "MA", lib)).unwrap();
        init::write_all(&dir, &files).unwrap();
        for f in files.iter().filter(|f| f.path.ends_with(".m31")) {
            let module = f.path.rsplit('/').next().unwrap().trim_end_matches(".m31");
            let again = crate::reformat(&f.text, module).expect("expected it to parse");
            assert_eq!(again, f.text, "{} is not fmt-stable", f.path);
        }
        let rc = lint::run_paths(&[dir.to_string_lossy().to_string()]);
        assert_eq!(
            format!("{rc:?}"),
            format!("{:?}", ExitCode::SUCCESS),
            "lint, lib={lib}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
