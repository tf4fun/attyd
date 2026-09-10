import os
import runpy
import subprocess
import sys
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/release-metadata.py"
release_metadata = runpy.run_path(str(SCRIPT))["release_metadata"]


class ReleaseMetadataTests(unittest.TestCase):
    def test_tag_overrides_development_version(self):
        self.assertEqual(
            release_metadata("tag", "v2.3.4", "0.1.0"),
            {"version": "2.3.4", "prerelease": "false"},
        )

    def test_prerelease_and_build_metadata_are_distinct(self):
        for version, prerelease in [
            ("0.0.0", "false"),
            ("1.2.3-rc.1", "true"),
            ("1.2.3-0+build.007", "true"),
            ("1.2.3+build-with-hyphens.007", "false"),
        ]:
            with self.subTest(version=version):
                self.assertEqual(
                    release_metadata("tag", f"v{version}", "0.1.0"),
                    {"version": version, "prerelease": prerelease},
                )

    def test_branch_and_pull_request_use_package_version(self):
        for ref_name in ["main", "v9.9.9", "42/merge"]:
            with self.subTest(ref_name=ref_name):
                self.assertEqual(
                    release_metadata("branch", ref_name, "0.1.0"),
                    {"version": "0.1.0", "prerelease": "false"},
                )

    def test_invalid_tags_cannot_be_published_or_inject_workflow_output(self):
        for tag in [
            "1.2.3", "v", "vnext", "v1.2", "v01.2.3", "v1.02.3", "v1.2.03",
            "v1.2.3-01", "v1.2.3-rc.01", "v1.2.3-", "v1.2.3+", "v1.2.3-rc..1",
            "v1.2.3\nprerelease=false", "v1.2.3\n", "v1.2.3$(id)", "v1.2.3/extra",
        ]:
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                release_metadata("tag", tag, "0.1.0")

    def test_cli_emits_only_validated_github_outputs(self):
        for tag, expected in [
            ("v2.3.4-rc.1", "version=2.3.4-rc.1\nprerelease=true\n"),
            ("v2.3.4\nprerelease=true", None),
        ]:
            with self.subTest(tag=tag):
                result = subprocess.run(
                    [sys.executable, str(SCRIPT)], cwd=ROOT, capture_output=True, text=True,
                    env={**os.environ, "GITHUB_REF_TYPE": "tag", "GITHUB_REF_NAME": tag},
                    check=False,
                )
                if expected is None:
                    self.assertNotEqual(result.returncode, 0)
                    self.assertEqual(result.stdout, "")
                else:
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stdout, expected)


if __name__ == "__main__":
    unittest.main()
