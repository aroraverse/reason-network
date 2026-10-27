#!/bin/sh
# Tests for install.sh against local release fixtures.
#
#   sh scripts/test_install.sh
#
# Builds fake releases with scripts/package.py (the "executable" is a small
# shell script that answers --version), then installs into temporary
# directories with a temporary HOME and data directory. Nothing outside the
# temporary directory is touched and nothing is downloaded.

set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
INSTALLER="${ROOT}/install.sh"
PYTHON="${PYTHON:-$(command -v python3 || command -v python)}"
WORK="$(mktemp -d 2>/dev/null || mktemp -d -t rn-install-test)"
trap 'rm -rf "$WORK"' EXIT INT TERM
TARGET="x86_64-unknown-linux-gnu"
PASSED=0

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    exit 1
}
pass() {
    PASSED=$((PASSED + 1))
    printf 'ok - %s\n' "$*"
}

# make_release <dir> <version>...: one package per version, plus SHA256SUMS.
make_release() {
    dir="$1"
    shift
    mkdir -p "$dir"
    for version in "$@"; do
        fake="${WORK}/fake-${version}"
        cat >"$fake" <<EOF
#!/bin/sh
case "\${1:-}" in
    --version) echo "reason-network ${version}" ;;
    service) echo "fake service \$2"; echo "service \$2" >>"\${HOME}/service.log" ;;
esac
EOF
        chmod 755 "$fake"
        "$PYTHON" "${ROOT}/scripts/package.py" package --binary "$fake" --target "$TARGET" \
            --version "$version" --out "$dir" >/dev/null
    done
    "$PYTHON" "${ROOT}/scripts/package.py" sums --out "$dir" >/dev/null
}

export HOME="${WORK}/home"
export REASON_NETWORK_DATA_DIR="${WORK}/data"
mkdir -p "$HOME" "$REASON_NETWORK_DATA_DIR"
echo "precious" >"${REASON_NETWORK_DATA_DIR}/reason-network.db"
echo '{"mcpServers":{"other":{}}}' >"${HOME}/.claude.json"
DEST="${WORK}/bin"

run() { sh "$INSTALLER" --target "$TARGET" --install-dir "$DEST" "$@"; }

data_intact() {
    [ "$(cat "${REASON_NETWORK_DATA_DIR}/reason-network.db")" = "precious" ] || fail "data was modified"
    [ "$(cat "${HOME}/.claude.json")" = '{"mcpServers":{"other":{}}}' ] || fail "configuration was modified"
}

make_release "${WORK}/rel" 0.1.0 0.2.0

# Fresh install picks the newest version.
out="$(run --from-dir "${WORK}/rel")"
[ "$("${DEST}/reason-network" --version)" = "reason-network 0.2.0" ] || fail "latest not installed: $out"
echo "$out" | grep -q "verified SHA-256" || fail "no verification message"
echo "$out" | grep -q "is not on your PATH" || fail "no PATH hint"
data_intact
pass "fresh install selects the latest version and explains PATH"

# Running again is safe.
run --from-dir "${WORK}/rel" >/dev/null
run --from-dir "${WORK}/rel" >/dev/null
[ "$("${DEST}/reason-network" --version)" = "reason-network 0.2.0" ] || fail "re-run broke the install"
[ -z "$(find "$DEST" -name '.reason-network.new.*')" ] || fail "staged file left behind"
pass "repeated install is idempotent"

# A specific version (downgrade), then an upgrade back.
run --from-dir "${WORK}/rel" --version 0.1.0 >/dev/null
[ "$("${DEST}/reason-network" --version)" = "reason-network 0.1.0" ] || fail "pinned version not installed"
out="$(run --from-dir "${WORK}/rel" --version v0.2.0)"
echo "$out" | grep -q "upgraded reason-network 0.1.0 -> reason-network 0.2.0" || fail "no upgrade message: $out"
data_intact
pass "specific versions and upgrades preserve data and configuration"

# Unknown version.
if out="$(run --from-dir "${WORK}/rel" --version 9.9.9 2>&1)"; then fail "unknown version accepted"; fi
echo "$out" | grep -q "not listed in SHA256SUMS" || fail "unclear error: $out"
pass "an unknown version is rejected"

# Tampered package: rejected, existing install untouched.
cp -R "${WORK}/rel" "${WORK}/tampered"
printf 'x' >>"${WORK}/tampered/reason-network-0.1.0-${TARGET}.tar.gz"
before="$("${DEST}/reason-network" --version)"
if out="$(run --from-dir "${WORK}/tampered" --version 0.1.0 2>&1)"; then fail "tampered package accepted"; fi
echo "$out" | grep -q "checksum mismatch" || fail "unclear error: $out"
[ "$("${DEST}/reason-network" --version)" = "$before" ] || fail "install changed after a failed verification"
pass "a tampered package is rejected without touching the install"

# Missing dependency: no tar on PATH.
mkdir -p "${WORK}/nopath"
for tool in sh cat sed awk sort tail tr cp mv rm mkdir chmod mktemp uname grep dirname find printf; do
    path="$(command -v "$tool" 2>/dev/null || true)"
    if [ -n "$path" ] && [ -f "$path" ]; then ln -sf "$path" "${WORK}/nopath/${tool}" 2>/dev/null || cp "$path" "${WORK}/nopath/"; fi
done
SH="$(command -v sh)"
if out="$(PATH="${WORK}/nopath" "$SH" "$INSTALLER" --target "$TARGET" --install-dir "$DEST" --from-dir "${WORK}/rel" 2>&1)"; then
    fail "missing tar not detected"
fi
echo "$out" | grep -q "missing dependency: tar" || fail "unclear error: $out"
pass "a missing dependency is reported"

# Downloads need a repository.
if out="$(run 2>&1)"; then fail "no repository accepted"; fi
echo "$out" | grep -q "no repository given" || fail "unclear error: $out"
pass "downloading without a repository explains what to pass"

# Unsupported platform.
if out="$(sh "$INSTALLER" --target riscv64-unknown-linux-gnu --install-dir "$DEST" --from-dir "${WORK}/rel" 2>&1)"; then
    fail "unsupported target accepted"
fi
echo "$out" | grep -q "unsupported target" || fail "unclear error: $out"
case "$(uname -s)/$(uname -m)" in
    Linux/x86_64 | Darwin/*) ;;
    *)
        if out="$(sh "$INSTALLER" --install-dir "$DEST" --from-dir "${WORK}/rel" 2>&1)"; then fail "unsupported host accepted"; fi
        echo "$out" | grep -q "no release is built for" || fail "unclear error: $out"
        ;;
esac
pass "unsupported platforms are rejected"

# Uninstall keeps data, removes the service through the executable.
out="$(run --uninstall)"
[ ! -e "${DEST}/reason-network" ] || fail "executable not removed"
grep -q "service uninstall" "${HOME}/service.log" || fail "service not unregistered"
echo "$out" | grep -q "data directory kept" || fail "no data message: $out"
data_intact
out="$(run --uninstall)"
echo "$out" | grep -q "is not installed" || fail "second uninstall unclear: $out"
pass "uninstall removes the executable and service but keeps data"

# Purging requires the explicit flag.
if run --purge-data --from-dir "${WORK}/rel" >/dev/null 2>&1; then fail "--purge-data accepted without --uninstall"; fi
run --uninstall --purge-data >/dev/null
[ ! -e "$REASON_NETWORK_DATA_DIR" ] || fail "data not purged"
[ -f "${HOME}/.claude.json" ] || fail "agent configuration removed"
pass "--purge-data deletes only the data directory"

printf '\n%d install.sh tests passed\n' "$PASSED"
