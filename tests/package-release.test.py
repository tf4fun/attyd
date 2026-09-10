import hashlib
import os
import stat
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "scripts/package-release.py"
ARCHIVES = {
    "x86_64-unknown-linux-gnu": "attyd-x86_64-unknown-linux-gnu.tar.gz",
    "aarch64-unknown-linux-gnu": "attyd-aarch64-unknown-linux-gnu.tar.gz",
    "x86_64-unknown-linux-musl": "attyd-x86_64-unknown-linux-musl.tar.gz",
    "aarch64-unknown-linux-musl": "attyd-aarch64-unknown-linux-musl.tar.gz",
    "x86_64-apple-darwin": "attyd-x86_64-apple-darwin.tar.gz",
    "aarch64-apple-darwin": "attyd-aarch64-apple-darwin.tar.gz",
    "x86_64-pc-windows-msvc": "attyd-x86_64-pc-windows-msvc.zip",
}


class PackageReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.output = self.root / "artifacts"
        (self.root / "LICENSE").write_bytes(b"Project license\n")
        for target in ARCHIVES:
            release = self.root / "target" / target / "release"
            release.mkdir(parents=True)
            binary = release / ("attyd.exe" if "windows" in target else "attyd")
            binary.write_bytes(f"Executable for {target}\n".encode())
            binary.chmod(0o751)
            (release / "THIRD_PARTY_LICENSES.txt").write_bytes(f"Licenses for {target}\n".encode())

    def run_script(self, *args, cwd=None):
        return subprocess.run(
            [sys.executable, str(SCRIPT), *map(str, args)],
            cwd=cwd or self.root, capture_output=True, text=True, check=False,
        )

    def test_seven_archives_contain_flat_files_and_binary_checksums(self):
        for target, archive_name in ARCHIVES.items():
            with self.subTest(target=target):
                result = self.run_script(target)
                self.assertEqual(result.returncode, 0, result.stderr)
                archive = self.output / archive_name
                self.assertTrue(archive.is_file())
                binary_name = "attyd.exe" if archive.suffix == ".zip" else "attyd"
                release = self.root / "target" / target / "release"
                binary = (release / binary_name).read_bytes()
                expected = {
                    binary_name: binary,
                    f"{binary_name}.sha256": f"{hashlib.sha256(binary).hexdigest()}  {binary_name}\n".encode(),
                    "LICENSE": (self.root / "LICENSE").read_bytes(),
                    "THIRD_PARTY_LICENSES.txt": (release / "THIRD_PARTY_LICENSES.txt").read_bytes(),
                }
                if archive.suffix == ".zip":
                    with zipfile.ZipFile(archive) as package:
                        self.assertEqual(package.namelist(), list(expected))
                        self.assertEqual({name: package.read(name) for name in package.namelist()}, expected)
                        self.assertIsNone(package.testzip())
                else:
                    with tarfile.open(archive, "r:gz") as package:
                        self.assertEqual(package.getnames(), list(expected))
                        self.assertTrue(all(member.isfile() for member in package.getmembers()))
                        self.assertEqual({name: package.extractfile(name).read() for name in package.getnames()}, expected)
                        mode = package.getmember(binary_name).mode
                        self.assertEqual(mode, stat.S_IMODE((release / binary_name).stat().st_mode))
                        if os.name != "nt":
                            self.assertEqual(mode, 0o751)
                self.assertFalse((release / f"{binary_name}.sha256").exists())
        self.assertEqual({path.name for path in self.output.iterdir()}, set(ARCHIVES.values()))

    def test_root_and_output_can_be_overridden(self):
        custom_output = self.root / "custom output"
        result = self.run_script(
            "x86_64-apple-darwin", "--root", self.root, "--output", custom_output, cwd=SCRIPT.parent,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((custom_output / ARCHIVES["x86_64-apple-darwin"]).is_file())
        self.assertFalse(self.output.exists())

    def test_root_override_sets_default_output(self):
        result = self.run_script("x86_64-apple-darwin", "--root", self.root, cwd=SCRIPT.parent)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((self.output / ARCHIVES["x86_64-apple-darwin"]).is_file())

    def test_missing_or_empty_inputs_fail_without_creating_an_archive(self):
        target = "x86_64-unknown-linux-gnu"
        release = self.root / "target" / target / "release"
        for path in [release / "attyd", release / "THIRD_PARTY_LICENSES.txt", self.root / "LICENSE"]:
            data = path.read_bytes()
            for state in ["missing", "empty"]:
                with self.subTest(input=path.name, state=state):
                    path.unlink()
                    if state == "empty":
                        path.touch()
                    result = self.run_script(target)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn(str(path), result.stderr)
                    self.assertEqual(result.stdout, "")
                    self.assertFalse(self.output.exists())
                    path.write_bytes(data)

    def test_checksums_cover_exactly_seven_full_archive_names(self):
        self.output.mkdir()
        expected = []
        for archive_name in ARCHIVES.values():
            data = f"Archive contents for {archive_name}\n".encode()
            (self.output / archive_name).write_bytes(data)
            expected.append(f"{hashlib.sha256(data).hexdigest()}  {archive_name}\n")
        (self.output / "unrelated.tar.gz").write_bytes(b"Ignored")
        result = self.run_script("--checksums")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.output / "SHA256SUMS").read_bytes(), "".join(expected).encode())

    def test_checksums_output_can_be_overridden(self):
        custom_output = self.root / "custom output"
        custom_output.mkdir()
        for archive_name in ARCHIVES.values():
            (custom_output / archive_name).write_bytes(b"Archive")
        result = self.run_script("--checksums", "--output", custom_output)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len((custom_output / "SHA256SUMS").read_text().splitlines()), 7)

    def test_checksums_reject_missing_or_empty_archives(self):
        self.output.mkdir()
        for archive_name in ARCHIVES.values():
            (self.output / archive_name).write_bytes(b"Archive")
        for archive_name in ARCHIVES.values():
            path = self.output / archive_name
            for state in ["missing", "empty"]:
                with self.subTest(archive=archive_name, state=state):
                    path.unlink()
                    if state == "empty":
                        path.touch()
                    result = self.run_script("--checksums")
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn(str(path), result.stderr)
                    self.assertEqual(result.stdout, "")
                    self.assertFalse((self.output / "SHA256SUMS").exists())
                    path.write_bytes(b"Archive")

    def test_cli_requires_exactly_one_supported_mode(self):
        for args in [(), ("--checksums", "x86_64-apple-darwin"), ("aarch64-pc-windows-msvc",), ("x86_64-unknown-freebsd",)]:
            with self.subTest(args=args):
                result = self.run_script(*args)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(result.stdout, "")
                self.assertFalse(self.output.exists())


if __name__ == "__main__":
    unittest.main()
