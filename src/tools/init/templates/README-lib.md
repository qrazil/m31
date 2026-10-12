# @@NAME@@

What it is, in a sentence. A library for [m31](https://github.com/qrazil/m31).

## Use

In a consumer's `deps`, pinned to a commit:

```
@@IDENT@@ https://github.com/qrazil/@@NAME@@ <commit>
```

then `import @@IDENT@@.core.@@PREFIX@@_core;`. The name on the left is the consumer's
choice and must be an identifier. A library with no `deps` entries of
its own needs nothing else.

## Test

You need `m31c` and the m31 runtime (a checkout of qrazil/m31, or an extracted
release).

```
M31_ROOT=/path/to/m31 LANGC=/path/to/m31c bash tests/test.sh
```

Checks formatting (`m31c fmt --check -r .`), the naming lint (`m31c lint .`) and
the unit tests in `tests/t_*.m31`. CI runs the same on Linux x86-64,
Linux aarch64 and macOS arm64.

## Layout

```
core/                  modules, named @@PREFIX@@_name
tests/                 test.sh runs everything; t_*.m31 are the unit tests
docs/                  design notes
deps                   project file: name, version, dependencies
RENAMES.md             renames and moves that break callers, by release
.github/workflows/     test.yml on every push, release.yml on a v* tag
```

Module names are global to a program, so every module here is prefixed
`@@PREFIX@@_`. Folders are for finding files.

## Release

Add an entry to `RENAMES.md`, bump `version` in `deps`, then:

```
git tag v@@VERSION@@ && git push origin v@@VERSION@@
```

A layout change that breaks callers is a minor-version bump before 1.0.
