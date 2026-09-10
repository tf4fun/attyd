"""Resolve the executable version and release channel for GitHub Actions."""

import os
import re
import tomllib
from pathlib import Path


SEMVER = re.compile(
    r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
    r"(?:-(?P<prerelease>[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
)


def release_metadata(ref_type, ref_name, package_version):
    version = package_version
    if ref_type == "tag":
        if not ref_name.startswith("v"):
            raise ValueError(f"Release tag must use v<SemVer>: {ref_name!r}")
        version = ref_name[1:]
    match = SEMVER.fullmatch(version)
    if not match:
        raise ValueError(f"Invalid semantic version: {version!r}")
    prerelease = match.group("prerelease")
    if prerelease and any(
        part.isdigit() and len(part) > 1 and part.startswith("0")
        for part in prerelease.split(".")
    ):
        raise ValueError(f"Numeric prerelease identifiers cannot have leading zeros: {version!r}")
    return {"version": version, "prerelease": "true" if prerelease else "false"}


if __name__ == "__main__":
    package_version = tomllib.loads(Path("Cargo.toml").read_text())["package"]["version"]
    try:
        metadata = release_metadata(
            os.environ.get("GITHUB_REF_TYPE", "branch"),
            os.environ.get("GITHUB_REF_NAME", ""),
            package_version,
        )
    except ValueError as error:
        raise SystemExit(str(error)) from error
    for name, value in metadata.items():
        print(f"{name}={value}")
