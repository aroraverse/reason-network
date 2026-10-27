#!/usr/bin/env python3
"""Build Reason Network release artifacts (standard library only).

    python scripts/package.py package --binary PATH --target TRIPLE [--version X.Y.Z] [--out dist]
    python scripts/package.py sums [--out dist]

`package` writes, for one target:

    reason-network-<version>-<target>.zip|.tar.gz   executable + README.md + INSTALL.md + VERSION
    reason-network-<version>-<target>[.exe]          bare executable, used by `reason-network update`

`sums` writes SHA256SUMS (sha256sum format) for every release file in the
output directory, including the installer scripts.

The naming scheme must match `src/update.rs`; both are covered by tests.
"""

from __future__ import annotations

import argparse
import datetime
import hashlib
import io
import os
import re
import shutil
import sys
import tarfile
import zipfile
from pathlib import Path

PRODUCT = "reason-network"
ROOT = Path(__file__).resolve().parent.parent
TARGETS = (
    "x86_64-pc-windows-msvc",
    "x86_64-unknown-linux-gnu",
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
)
DOCS = ("README.md", "INSTALL.md")
INSTALLERS = ("install.sh", "install.ps1")
VERSION_RE = re.compile(r"^\d+\.\d+\.\d+(-[0-9A-Za-z.]+)?$")


class PackagingError(Exception):
    """A user-facing packaging failure."""


def is_windows(target: str) -> bool:
    return "windows" in target


def package_name(version: str, target: str) -> str:
    return f"{PRODUCT}-{version}-{target}.{'zip' if is_windows(target) else 'tar.gz'}"


def binary_name(version: str, target: str) -> str:
    return f"{PRODUCT}-{version}-{target}{'.exe' if is_windows(target) else ''}"


def cargo_version(root: Path = ROOT) -> str:
    """The `version` of the [package] table in Cargo.toml."""
    in_package = False
    for line in (root / "Cargo.toml").read_text(encoding="utf-8").splitlines():
        stripped = line.strip()
        if stripped.startswith("["):
            in_package = stripped == "[package]"
        elif in_package:
            match = re.match(r'version\s*=\s*"([^"]+)"', stripped)
            if match:
                return match.group(1)
    raise PackagingError("no [package] version in Cargo.toml")


def version_file(version: str, target: str, commit: str | None) -> str:
    built = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    return (
        f"name: {PRODUCT}\nversion: {version}\ntarget: {target}\n"
        f"commit: {commit or 'unknown'}\nbuilt: {built}\n"
    )


def _add_file_to_tar(archive: tarfile.TarFile, name: str, data: bytes, mode: int) -> None:
    info = tarfile.TarInfo(name)
    info.size = len(data)
    info.mode = mode
    info.mtime = int(datetime.datetime.now().timestamp())
    archive.addfile(info, io.BytesIO(data))


def package(
    binary: Path,
    target: str,
    version: str,
    out: Path,
    commit: str | None = None,
    docs_root: Path = ROOT,
) -> list[Path]:
    """Write the package archive and the bare executable; return their paths."""
    if target not in TARGETS:
        raise PackagingError(f"unknown target {target}; expected one of {', '.join(TARGETS)}")
    if not VERSION_RE.match(version):
        raise PackagingError(f"invalid version {version}")
    if not binary.is_file():
        raise PackagingError(f"binary {binary} does not exist (build it first)")
    for doc in DOCS:
        if not (docs_root / doc).is_file():
            raise PackagingError(f"missing {doc} in {docs_root}")

    out.mkdir(parents=True, exist_ok=True)
    exe_name = PRODUCT + (".exe" if is_windows(target) else "")
    top = f"{PRODUCT}-{version}-{target}"
    binary_bytes = binary.read_bytes()
    files = [(exe_name, binary_bytes, 0o755)]
    files += [(doc, (docs_root / doc).read_bytes(), 0o644) for doc in DOCS]
    files.append(("VERSION", version_file(version, target, commit).encode(), 0o644))

    archive_path = out / package_name(version, target)
    if is_windows(target):
        with zipfile.ZipFile(archive_path, "w", compression=zipfile.ZIP_DEFLATED) as archive:
            for name, data, mode in files:
                info = zipfile.ZipInfo(f"{top}/{name}", date_time=datetime.datetime.now().timetuple()[:6])
                info.external_attr = (mode | 0o100000) << 16
                info.compress_type = zipfile.ZIP_DEFLATED
                archive.writestr(info, data)
    else:
        with tarfile.open(archive_path, "w:gz") as archive:
            for name, data, mode in files:
                _add_file_to_tar(archive, f"{top}/{name}", data, mode)

    bare = out / binary_name(version, target)
    shutil.copyfile(binary, bare)
    if not is_windows(target):
        bare.chmod(0o755)
    return [archive_path, bare]


def is_release_file(name: str) -> bool:
    return (name.startswith(f"{PRODUCT}-") or name in INSTALLERS) and name != "SHA256SUMS"


def write_sums(out: Path, include_installers_from: Path | None = ROOT) -> Path:
    """Copy the installers into `out` and write SHA256SUMS for all release files."""
    if include_installers_from is not None:
        for installer in INSTALLERS:
            source = include_installers_from / installer
            if source.is_file():
                shutil.copyfile(source, out / installer)
    lines = []
    for path in sorted(out.iterdir()):
        if path.is_file() and is_release_file(path.name):
            digest = hashlib.sha256(path.read_bytes()).hexdigest()
            lines.append(f"{digest}  {path.name}")
    if not lines:
        raise PackagingError(f"no release files in {out}")
    sums = out / "SHA256SUMS"
    sums.write_text("\n".join(lines) + "\n", encoding="utf-8", newline="\n")
    return sums


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    pack = sub.add_parser("package", help="package one target")
    pack.add_argument("--binary", required=True, type=Path)
    pack.add_argument("--target", required=True)
    pack.add_argument("--version", default=None)
    pack.add_argument("--out", default=Path("dist"), type=Path)
    pack.add_argument("--commit", default=os.environ.get("GITHUB_SHA"))
    sums = sub.add_parser("sums", help="write SHA256SUMS")
    sums.add_argument("--out", default=Path("dist"), type=Path)
    args = parser.parse_args(argv)
    try:
        if args.command == "package":
            version = args.version or cargo_version()
            for path in package(args.binary, args.target, version, args.out, args.commit):
                print(path)
        else:
            print(write_sums(args.out))
    except PackagingError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
