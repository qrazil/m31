#!/bin/sh
# Install the latest m31 release for this machine, no tarball juggling:
#
#   curl -fsSL https://raw.githubusercontent.com/qrazil/m31/master/install.sh | sh
#
# Picks the asset for this OS and CPU (Linux x86-64 / aarch64, macOS arm64;
# on a musl system -- Alpine -- the -musl Linux asset, falling back to the
# glibc one for a release that predates it),
# unpacks it under $PREFIX/opt/m31/<version> and links the programs into
# $PREFIX/bin. PREFIX defaults to ~/.local. Re-running upgrades; old versions
# stay in opt/ until you delete them.
#
#   VERSION=v1.2.3   install that release instead of the newest
#   PREFIX=/usr/local  install elsewhere (needs write access)
#   LIBC=musl|gnu    force the C library flavour instead of detecting it
#   sh install.sh --uninstall   remove the links and this program's opt/ dir
set -eu

REPO=m31
ASSET=m31          # release asset prefix: <ASSET>-<os>-<arch>.tar.gz
BINS="m31c"          # programs linked into $PREFIX/bin
PREFIX=${PREFIX:-$HOME/.local}
OPT="$PREFIX/opt/$REPO"
BIN="$PREFIX/bin"

say() { printf '%s\n' "$*"; }
die() { printf 'install: %s\n' "$*" >&2; exit 1; }

if [ "${1:-}" = "--uninstall" ]; then
    for b in $BINS; do
        [ -L "$BIN/$b" ] && rm -f "$BIN/$b" && say "removed $BIN/$b"
    done
    rm -rf "$OPT" && say "removed $OPT"
    exit 0
fi

command -v curl >/dev/null 2>&1 || die "curl is required"
command -v tar  >/dev/null 2>&1 || die "tar is required"

case "$(uname -s)" in
    Linux)  os=linux ;;
    Darwin) os=macos ;;
    *) die "unsupported OS: $(uname -s) (releases exist for Linux and macOS)" ;;
esac
case "$(uname -m)" in
    x86_64|amd64)  arch=x86_64 ;;
    aarch64|arm64) arch=aarch64 ;;
    *) die "unsupported CPU: $(uname -m)" ;;
esac
[ "$os" = macos ] && arch=arm64
[ "$os" = macos ] && [ "$(uname -m)" != arm64 ] && die "macOS releases are Apple silicon (arm64) only"

if [ -z "${VERSION:-}" ]; then
    # The newest release, prereleases included (the /latest alias skips them).
    VERSION=$(curl -fsSL "https://api.github.com/repos/qrazil/$REPO/releases?per_page=1" |
        sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)
    [ -n "$VERSION" ] || die "could not find a release of $REPO"
fi

# C library flavour on Linux: musl (Alpine) gets its own asset, whose compiler
# is a static binary. Detected from /etc/alpine-release or ldd's banner
# (musl's ldd prints "musl libc" on stderr, exit status 1).
libc=${LIBC:-}
if [ -z "$libc" ] && [ "$os" = linux ]; then
    libc=gnu
    if [ -e /etc/alpine-release ] ||
        { ldd --version 2>&1 || true; } | grep -qi musl; then
        libc=musl
    fi
fi
base="https://github.com/qrazil/$REPO/releases/download/$VERSION"
file="$ASSET-$os-$arch.tar.gz"
if [ "$libc" = musl ] && [ "$os" = linux ]; then
    file="$ASSET-$os-$arch-musl.tar.gz"
    # Releases before the musl assets existed: the glibc tarball's compiler
    # does not run on musl, but its SDK does not care, so say so and carry on
    # only if the download exists.
    if ! curl -fsSLI "$base/$file" >/dev/null 2>&1; then
        say "no musl asset in $VERSION; using the glibc one"
        file="$ASSET-$os-$arch.tar.gz"
    fi
fi
url="$base/$file"
dest="$OPT/$VERSION"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/$REPO-install.XXXXXX")
trap 'rm -rf "$tmp"' EXIT INT TERM

say "$REPO $VERSION ($os $arch${libc:+ $libc})"
curl -fsSL "$url" -o "$tmp/$file" || die "download failed: $url"
mkdir -p "$tmp/x" "$BIN" "$OPT"
tar -xzf "$tmp/$file" -C "$tmp/x" || die "could not unpack $file"

rm -rf "$dest"
mv "$tmp/x" "$dest"
for b in $BINS; do
    [ -x "$dest/$b" ] || die "$b is not in the release ($dest)"
    ln -sf "$dest/$b" "$BIN/$b"
    say "linked $BIN/$b"
done

case ":$PATH:" in
    *":$BIN:"*) ;;
    *) say "add $BIN to your PATH:  export PATH=\"$BIN:\$PATH\"" ;;
esac

say "to build programs against this release:  export M31_ROOT=\"$dest\""
