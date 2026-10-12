# @@NAME@@

What it is, in a sentence. Written in [m31](https://github.com/qrazil/m31).

```
@@NAME@@            prints "hello, world"
@@NAME@@ you        prints "hello, you"
```

## Build

You need `m31c` and the m31 runtime (a checkout of qrazil/m31, or an extracted
release; the release tarball has both).

```
M31_ROOT=/path/to/m31 LANGC=/path/to/m31c bash scripts/build.sh      # -> ./@@NAME@@
```

## Test

```
M31_ROOT=/path/to/m31 LANGC=/path/to/m31c bash tests/test.sh
```

Checks formatting (`m31c fmt --check -r .`), the naming lint (`m31c lint .`),
the unit tests in `tests/t_*.m31` and the built program. CI runs the same on
Linux x86-64, Linux aarch64 and macOS arm64.

## Layout

```
@@IDENT@@.m31          entry point
core/                  modules, named @@PREFIX@@_name
tests/                 test.sh runs everything; t_*.m31 are the unit tests
scripts/build.sh       compile and link the binary
docs/                  design notes
deps                   project file: name, version, dependencies
RENAMES.md             renames and moves that break callers, by release
.github/workflows/     test.yml on every push, release.yml on a v* tag
```

Module names are global to a program, so every module here is prefixed
`@@PREFIX@@_`. Folders are for finding files; `import core.@@PREFIX@@_core;`
works from any file in the project.

## Release

Add an entry to `RENAMES.md`, bump `version` in `deps`, then:

```
git tag v@@VERSION@@ && git push origin v@@VERSION@@
```

The release workflow tests on all three platforms, builds the binaries and
attaches them to a GitHub Release. A layout change that breaks callers is a
minor-version bump before 1.0.
