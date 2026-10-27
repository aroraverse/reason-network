#!/bin/sh
# Reason Network installer for macOS and Linux.
#
#   sh install.sh [options]
#
# Options:
#   --version X.Y.Z     install this version (default: the latest release)
#   --install-dir DIR   where to put the executable (default: ~/.local/bin)
#   --from-dir DIR      install from a local directory holding SHA256SUMS and the
#                       release packages, instead of downloading
#   --repo OWNER/NAME   GitHub repository to download from
#                       (default: $REASON_NETWORK_REPO)
#   --target TRIPLE     override platform detection (testing)
#   --uninstall         remove the executable and its service; keep user data
#   --purge-data        with --uninstall: also delete the data directory
#   -h, --help          show this help
#
# Every package is verified against the release's SHA256SUMS before anything
# is installed. Configuration and application data are never modified by an
# install or upgrade. No administrator rights are needed.

set -eu

PRODUCT="reason-network"
VERSION=""
INSTALL_DIR="${REASON_NETWORK_INSTALL_DIR:-${HOME}/.local/bin}"
FROM_DIR=""
REPO="${REASON_NETWORK_REPO:-}"
TARGET=""
UNINSTALL=0
PURGE=0
TMP=""

say() { printf '%s\n' "$*"; }
fail() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}
cleanup() { if [ -n "$TMP" ] && [ -d "$TMP" ]; then rm -rf "$TMP"; fi; }
trap cleanup EXIT INT TERM

usage() { sed -n '2,21p' "$0" | sed 's/^# \{0,1\}//'; }

need_value() { if [ "$#" -lt 2 ] || [ -z "$2" ]; then fail "$1 needs a value"; fi; }

while [ "$#" -gt 0 ]; do
    case "$1" in
        --version) need_value "$@"; VERSION="${2#v}"; shift 2 ;;
        --install-dir) need_value "$@"; INSTALL_DIR="$2"; shift 2 ;;
        --from-dir) need_value "$@"; FROM_DIR="$2"; shift 2 ;;
        --repo) need_value "$@"; REPO="$2"; shift 2 ;;
        --target) need_value "$@"; TARGET="$2"; shift 2 ;;
        --uninstall) UNINSTALL=1; shift ;;
        --purge-data) PURGE=1; shift ;;
        -h | --help) usage; exit 0 ;;
        *) fail "unknown option $1 (see --help)" ;;
    esac
done

BIN="${INSTALL_DIR}/${PRODUCT}"

data_dir() {
    if [ -n "${REASON_NETWORK_DATA_DIR:-}" ]; then
        printf '%s\n' "$REASON_NETWORK_DATA_DIR"
    elif [ -n "${XDG_DATA_HOME:-}" ]; then
        printf '%s\n' "${XDG_DATA_HOME}/${PRODUCT}"
    else
        printf '%s\n' "${HOME}/.local/share/${PRODUCT}"
    fi
}

# ---------------------------------------------------------------- uninstall
if [ "$UNINSTALL" -eq 1 ]; then
    if [ -x "$BIN" ]; then
        # Stop and unregister the background service; this never deletes data.
        "$BIN" service uninstall || say "warning: could not remove the background service; remove it manually if it exists"
        rm -f "$BIN" "${BIN}.old"
        say "removed ${BIN}"
    else
        say "${BIN} is not installed"
    fi
    DATA="$(data_dir)"
    if [ "$PURGE" -eq 1 ]; then
        if [ -d "$DATA" ]; then
            rm -rf "$DATA"
            say "deleted data directory ${DATA}"
        fi
    else
        say "data directory kept: ${DATA} (pass --purge-data to delete it)"
    fi
    say "agent configurations may still list the MCP server; remove it with"
    say "  claude mcp remove reason-network --scope user"
    say "  codex mcp remove reason-network"
    exit 0
fi
[ "$PURGE" -eq 0 ] || fail "--purge-data is only valid with --uninstall"

# ----------------------------------------------------------------- platform
if [ -z "$TARGET" ]; then
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$arch" in
        x86_64 | amd64) arch="x86_64" ;;
        arm64 | aarch64) arch="aarch64" ;;
    esac
    case "${os}/${arch}" in
        Linux/x86_64) TARGET="x86_64-unknown-linux-gnu" ;;
        Darwin/aarch64) TARGET="aarch64-apple-darwin" ;;
        Darwin/x86_64) TARGET="x86_64-apple-darwin" ;;
        *) fail "no release is built for ${os}/${arch}; supported: Linux x86_64, macOS arm64 and x86_64 (build from source with cargo instead)" ;;
    esac
fi
case "$TARGET" in
    x86_64-unknown-linux-gnu | aarch64-apple-darwin | x86_64-apple-darwin) ;;
    *windows*) fail "use install.ps1 on Windows" ;;
    *) fail "unsupported target ${TARGET}" ;;
esac

# ------------------------------------------------------------- dependencies
have() { command -v "$1" >/dev/null 2>&1; }
have tar || fail "missing dependency: tar (install it with your package manager)"
if have sha256sum; then
    sha256() { sha256sum "$1" | awk '{print $1}'; }
elif have shasum; then
    sha256() { shasum -a 256 "$1" | awk '{print $1}'; }
else
    fail "missing dependency: sha256sum or shasum (install coreutils or perl)"
fi
if [ -z "$FROM_DIR" ]; then
    if have curl; then
        download() { curl --proto '=https' --tlsv1.2 -fsSL "$1" -o "$2"; }
    elif have wget; then
        download() { wget -q --https-only "$1" -O "$2"; }
    else
        fail "missing dependency: curl or wget (or use --from-dir with downloaded files)"
    fi
    [ -n "$REPO" ] || fail "no repository given: pass --repo OWNER/NAME or set REASON_NETWORK_REPO (or use --from-dir)"
    case "$REPO" in
        */*) ;;
        *) fail "--repo must look like OWNER/NAME" ;;
    esac
fi

TMP="$(mktemp -d 2>/dev/null || mktemp -d -t reason-network)"

fetch() { # fetch <name> <dest> [<version>]
    if [ -n "$FROM_DIR" ]; then
        [ -f "${FROM_DIR}/$1" ] || fail "${FROM_DIR}/$1 does not exist"
        cp "${FROM_DIR}/$1" "$2"
    elif [ -n "${3:-}" ]; then
        download "https://github.com/${REPO}/releases/download/v$3/$1" "$2" ||
            fail "cannot download $1 for version $3 (does the release exist?)"
    else
        download "https://github.com/${REPO}/releases/latest/download/$1" "$2" ||
            fail "cannot download $1 from the latest release of ${REPO} (has a release been published?)"
    fi
}

# ------------------------------------------------------------------ version
fetch SHA256SUMS "${TMP}/SHA256SUMS" "$VERSION"
if [ -z "$VERSION" ]; then
    VERSION="$(sed -n "s/^[0-9a-fA-F]\{64\}  *\*\{0,1\}${PRODUCT}-\([0-9][0-9.]*[0-9]\(-[0-9A-Za-z.]*\)\{0,1\}\)-${TARGET}\.tar\.gz\$/\1/p" "${TMP}/SHA256SUMS" |
        sort -t. -k1,1n -k2,2n -k3,3n | tail -n 1)"
    [ -n "$VERSION" ] || fail "SHA256SUMS lists no package for ${TARGET}"
fi
case "$VERSION" in
    [0-9]*.[0-9]*.[0-9]*) ;;
    *) fail "invalid version ${VERSION}" ;;
esac
PACKAGE="${PRODUCT}-${VERSION}-${TARGET}.tar.gz"

# ----------------------------------------------------------------- download
EXPECTED="$(awk -v name="$PACKAGE" '{ file = $2; sub(/^\*/, "", file); if (file == name) print tolower($1) }' "${TMP}/SHA256SUMS")"
[ -n "$EXPECTED" ] || fail "${PACKAGE} is not listed in SHA256SUMS (version ${VERSION} has no ${TARGET} package)"
say "downloading ${PACKAGE}"
fetch "$PACKAGE" "${TMP}/${PACKAGE}" "$VERSION"
ACTUAL="$(sha256 "${TMP}/${PACKAGE}" | tr 'A-F' 'a-f')"
[ "$ACTUAL" = "$EXPECTED" ] || fail "checksum mismatch for ${PACKAGE}: expected ${EXPECTED}, got ${ACTUAL}; nothing was installed"
say "verified SHA-256 ${ACTUAL}"

# ------------------------------------------------------------------ install
tar -xzf "${TMP}/${PACKAGE}" -C "$TMP" || fail "cannot extract ${PACKAGE}"
NEW="${TMP}/${PRODUCT}-${VERSION}-${TARGET}/${PRODUCT}"
[ -f "$NEW" ] || fail "${PACKAGE} does not contain ${PRODUCT}"
chmod 755 "$NEW"
REPORTED="$("$NEW" --version 2>/dev/null || true)"
case "$REPORTED" in
    "${PRODUCT} "*) ;;
    *) fail "the downloaded executable does not run on this machine; nothing was installed" ;;
esac

mkdir -p "$INSTALL_DIR" || fail "cannot create ${INSTALL_DIR}"
[ -w "$INSTALL_DIR" ] || fail "${INSTALL_DIR} is not writable; choose another with --install-dir"
PREVIOUS=""
if [ -x "$BIN" ]; then PREVIOUS="$("$BIN" --version 2>/dev/null || true)"; fi
STAGED="${INSTALL_DIR}/.${PRODUCT}.new.$$"
if ! cp "$NEW" "$STAGED" || ! chmod 755 "$STAGED"; then
    rm -f "$STAGED"
    fail "cannot write to ${INSTALL_DIR}"
fi
# Renaming over the old file is atomic, and a running daemon keeps its copy.
mv -f "$STAGED" "$BIN" || {
    rm -f "$STAGED"
    fail "cannot replace ${BIN}"
}

if [ -n "$PREVIOUS" ]; then
    say "upgraded ${PREVIOUS} -> ${REPORTED} at ${BIN}"
else
    say "installed ${REPORTED} at ${BIN}"
fi
say "configuration and data were not modified (data directory: $(data_dir))"

case ":${PATH}:" in
    *":${INSTALL_DIR}:"*) ;;
    *)
        say ""
        say "${INSTALL_DIR} is not on your PATH. Add it, for example:"
        say "  echo 'export PATH=\"${INSTALL_DIR}:\$PATH\"' >> ~/.profile    # then open a new shell"
        ;;
esac
say ""
say "next steps:"
say "  ${PRODUCT} setup --agent all      # configure Claude Code and Codex"
say "  ${PRODUCT} service install && ${PRODUCT} service start"
say "  ${PRODUCT} doctor"
if [ -n "$PREVIOUS" ]; then
    say "  restart a running daemon to use the new version: ${PRODUCT} service stop && ${PRODUCT} service start"
fi
