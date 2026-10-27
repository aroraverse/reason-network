"""Tests for scripts/package.py.

    python -m unittest discover -s scripts -p "test_*.py"
"""

import hashlib
import sys
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import package  # noqa: E402


class PackagingTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.docs = self.root / "docs"
        self.docs.mkdir()
        (self.docs / "README.md").write_text("readme")
        (self.docs / "INSTALL.md").write_text("install")
        (self.docs / "install.sh").write_text("#!/bin/sh\n")
        (self.docs / "install.ps1").write_text("param()\n")
        self.binary = self.root / "reason-network-built"
        self.binary.write_bytes(b"binary bytes")
        self.out = self.root / "dist"

    def tearDown(self):
        self.tmp.cleanup()

    def test_names_match_the_rust_update_module(self):
        # Mirrors `artifact_names_round_trip` in src/update.rs.
        self.assertEqual(
            package.package_name("0.2.0", "x86_64-pc-windows-msvc"),
            "reason-network-0.2.0-x86_64-pc-windows-msvc.zip",
        )
        self.assertEqual(
            package.package_name("0.2.0", "aarch64-apple-darwin"),
            "reason-network-0.2.0-aarch64-apple-darwin.tar.gz",
        )
        self.assertEqual(
            package.binary_name("0.2.0", "x86_64-pc-windows-msvc"),
            "reason-network-0.2.0-x86_64-pc-windows-msvc.exe",
        )
        self.assertEqual(
            package.binary_name("0.2.0", "x86_64-unknown-linux-gnu"),
            "reason-network-0.2.0-x86_64-unknown-linux-gnu",
        )

    def test_unix_package_contents_and_permissions(self):
        target = "x86_64-unknown-linux-gnu"
        archive, bare = package.package(self.binary, target, "1.2.3", self.out, "abc", self.docs)
        self.assertEqual(archive.name, "reason-network-1.2.3-x86_64-unknown-linux-gnu.tar.gz")
        self.assertEqual(bare.read_bytes(), b"binary bytes")
        with tarfile.open(archive) as tar:
            names = sorted(tar.getnames())
            top = "reason-network-1.2.3-x86_64-unknown-linux-gnu"
            self.assertEqual(
                names,
                sorted(f"{top}/{name}" for name in ("reason-network", "README.md", "INSTALL.md", "VERSION")),
            )
            self.assertEqual(tar.getmember(f"{top}/reason-network").mode & 0o777, 0o755)
            version = tar.extractfile(f"{top}/VERSION").read().decode()
            self.assertIn("version: 1.2.3", version)
            self.assertIn(f"target: {target}", version)
            self.assertIn("commit: abc", version)

    def test_windows_package_is_a_zip_with_an_exe(self):
        archive, bare = package.package(
            self.binary, "x86_64-pc-windows-msvc", "1.2.3", self.out, None, self.docs
        )
        self.assertTrue(bare.name.endswith(".exe"))
        with zipfile.ZipFile(archive) as zf:
            top = "reason-network-1.2.3-x86_64-pc-windows-msvc"
            self.assertIn(f"{top}/reason-network.exe", zf.namelist())
            self.assertEqual(zf.read(f"{top}/reason-network.exe"), b"binary bytes")

    def test_sums_cover_every_release_file(self):
        for target in package.TARGETS:
            package.package(self.binary, target, "1.2.3", self.out, None, self.docs)
        (self.out / "unrelated.txt").write_text("not part of the release")
        sums = package.write_sums(self.out, self.docs)
        lines = sums.read_text().splitlines()
        self.assertEqual(len(lines), 4 * 2 + 2)  # archive + binary per target, two installers
        for line in lines:
            digest, name = line.split("  ")
            self.assertEqual(hashlib.sha256((self.out / name).read_bytes()).hexdigest(), digest)
            self.assertNotEqual(name, "unrelated.txt")
        self.assertIn("install.sh", sums.read_text())

    def test_errors_are_reported(self):
        with self.assertRaises(package.PackagingError):
            package.package(self.binary, "riscv64-unknown-linux-gnu", "1.2.3", self.out, None, self.docs)
        with self.assertRaises(package.PackagingError):
            package.package(self.binary, "x86_64-apple-darwin", "1.2", self.out, None, self.docs)
        with self.assertRaises(package.PackagingError):
            package.package(self.root / "missing", "x86_64-apple-darwin", "1.2.3", self.out, None, self.docs)
        empty = self.root / "empty"
        empty.mkdir()
        with self.assertRaises(package.PackagingError):
            package.write_sums(empty, None)

    def test_cargo_version_is_read_from_the_package_table(self):
        (self.root / "Cargo.toml").write_text(
            '[package]\nname = "x"\nversion = "3.4.5"\n\n[dependencies]\nfoo = { version = "9.9.9" }\n'
        )
        self.assertEqual(package.cargo_version(self.root), "3.4.5")
        self.assertRegex(package.cargo_version(), package.VERSION_RE)


if __name__ == "__main__":
    unittest.main()
