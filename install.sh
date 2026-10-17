#!/bin/sh
# kast installer: downloads a release binary from GitHub, verifies its SHA-256
# checksum and installs it.
#
#   curl -fsSL https://raw.githubusercontent.com/kud3n013/kast/main/install.sh | sh
#   sh install.sh --uninstall
#
# Environment:
#   KAST_VERSION      release tag to install, e.g. v0.2.0 (default: latest)
#   KAST_INSTALL_DIR  where to put the binary (default: /usr/local/bin)
#
# Only the final copy into KAST_INSTALL_DIR may use sudo, and only when that
# directory is not writable. kast itself is run with --version only.

set -eu

REPO="kud3n013/kast"
TMP_DIR=""

say() { printf '%s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

cleanup() {
    if [ -n "$TMP_DIR" ]; then
        rm -rf "$TMP_DIR"
    fi
}

need() {
    for cmd in "$@"; do
        command -v "$cmd" >/dev/null 2>&1 || die "'$cmd' is required but was not found; install it and try again"
    done
}

# Prints the release target for this machine.
detect_target() {
    os=$(uname -s)
    [ "$os" = "Linux" ] || die "kast runs on Linux only (this system is $os)"
    arch=$(uname -m)
    case "$arch" in
        x86_64 | amd64) say "x86_64-unknown-linux-musl" ;;
        aarch64 | arm64) say "aarch64-unknown-linux-musl" ;;
        *) die "no kast build for CPU architecture '$arch' (supported: x86_64, aarch64)" ;;
    esac
}

# Prints the base URL to download release assets from.
release_url() {
    version=${KAST_VERSION:-}
    if [ -z "$version" ]; then
        say "https://github.com/$REPO/releases/latest/download"
        return
    fi
    case "$version" in
        v*) ;;
        *) version="v$version" ;;
    esac
    say "https://github.com/$REPO/releases/download/$version"
}

download() {
    curl -fsSL --proto '=https' --tlsv1.2 -o "$2" "$1" || die "download failed: $1"
}

# Checks that file $1 matches the checksum line in $2 (sha256sum format).
verify() {
    expected=$(cut -d ' ' -f 1 <"$2")
    case "$expected" in
        *[!0-9a-f]*) die "malformed checksum file for $(basename "$1")" ;;
    esac
    [ "${#expected}" -eq 64 ] || die "malformed checksum file for $(basename "$1")"
    actual=$(sha256sum "$1" | cut -d ' ' -f 1)
    [ "$actual" = "$expected" ] || die "checksum mismatch for $(basename "$1") (expected $expected, got $actual); nothing was installed"
}

# Succeeds when $1, or the nearest existing parent of it, is writable.
writable() {
    dir=$1
    while [ ! -e "$dir" ]; do
        dir=$(dirname "$dir")
    done
    [ -d "$dir" ] && [ -w "$dir" ]
}

# Sets SUDO to "sudo" when $1 needs it, or exits when sudo is missing.
pick_sudo() {
    SUDO=""
    if writable "$1"; then
        return
    fi
    if command -v sudo >/dev/null 2>&1; then
        SUDO="sudo"
        return
    fi
    die "$1 is not writable and sudo is not available; try: KAST_INSTALL_DIR=\"\$HOME/.local/bin\" sh install.sh"
}

install_kast() {
    need curl sha256sum install uname mktemp cut
    target=$(detect_target)
    base=$(release_url)
    asset="kast-$target"

    TMP_DIR=$(mktemp -d)
    say "Downloading $asset from $base"
    download "$base/$asset" "$TMP_DIR/$asset"
    download "$base/$asset.sha256" "$TMP_DIR/$asset.sha256"
    verify "$TMP_DIR/$asset" "$TMP_DIR/$asset.sha256"
    say "Checksum OK"

    pick_sudo "$BIN_DIR"
    if [ -n "$SUDO" ]; then
        say "Installing to $BIN_DIR/kast (needs sudo)"
    else
        say "Installing to $BIN_DIR/kast"
    fi
    $SUDO install -Dm755 "$TMP_DIR/$asset" "$BIN_DIR/kast"

    say "Installed: $("$BIN_DIR/kast" --version)"
    if [ -e "$BIN_DIR/kas" ]; then
        say "note: the old 'kas' binary is no longer needed; remove it with: ${SUDO:+sudo }rm $BIN_DIR/kas"
    fi
    case ":$PATH:" in
        *":$BIN_DIR:"*) ;;
        *) warn "$BIN_DIR is not on your PATH; add it, or run $BIN_DIR/kast" ;;
    esac
}

uninstall_kast() {
    if [ -e "$BIN_DIR/kast" ]; then
        pick_sudo "$BIN_DIR"
        $SUDO rm -f "$BIN_DIR/kast"
        say "Removed $BIN_DIR/kast"
    else
        say "kast is not installed in $BIN_DIR"
    fi
    say "Logs and backups were left in place; delete them yourself if you no longer need them:"
    say "  /var/log/kast.log"
    say "  ~/.local/state/kast.log"
    say "  ~/.local/share/app-vault-sync/backups/           (backups made by kast backup)"
    say "  ~/.local/state/app-vault-sync/safety-rollbacks/  (copies kept before a restore)"
}

main() {
    BIN_DIR=${KAST_INSTALL_DIR:-/usr/local/bin}
    trap cleanup EXIT
    trap 'exit 1' HUP INT TERM
    case "${1:-}" in
        "") install_kast ;;
        --uninstall) uninstall_kast ;;
        -h | --help) say "usage: sh install.sh [--uninstall]   (env: KAST_VERSION, KAST_INSTALL_DIR)" ;;
        *) die "unknown option '$1' (use --uninstall, or no option to install)" ;;
    esac
}

main "$@"
