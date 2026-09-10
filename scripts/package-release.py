"""Package portable releases and generate checksums for the complete target set."""

import argparse
import hashlib
import io
import tarfile
import zipfile
from pathlib import Path


TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
)


def archive_name(target):
    suffix = ".zip" if target.endswith("-windows-msvc") else ".tar.gz"
    return f"attyd-{target}{suffix}"


def read_required(path):
    if not path.is_file():
        raise ValueError(f"Required file is missing: {path}")
    data = path.read_bytes()
    if not data:
        raise ValueError(f"Required file is empty: {path}")
    return data


def checksum_line(name, data):
    return f"{hashlib.sha256(data).hexdigest()}  {name}\n"


def package_release(target, root, output):
    windows = target.endswith("-windows-msvc")
    release = root / "target" / target / "release"
    binary = release / ("attyd.exe" if windows else "attyd")
    files = [binary, root / "LICENSE", release / "THIRD_PARTY_LICENSES.txt"]
    entries = [(path.name, read_required(path), path.stat().st_mode & 0o777) for path in files]
    entries.insert(1, (f"{binary.name}.sha256", checksum_line(binary.name, entries[0][1]).encode(), 0o644))
    output.mkdir(parents=True, exist_ok=True)
    archive = output / archive_name(target)
    if windows:
        with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as package:
            for name, data, _ in entries:
                package.writestr(name, data)
    else:
        with tarfile.open(archive, "w:gz") as package:
            for name, data, mode in entries:
                entry = tarfile.TarInfo(name)
                entry.size = len(data)
                entry.mode = mode
                package.addfile(entry, io.BytesIO(data))
    return archive


def write_checksums(output):
    lines = [
        checksum_line(archive_name(target), read_required(output / archive_name(target)))
        for target in TARGETS
    ]
    checksum_file = output / "SHA256SUMS"
    checksum_file.write_text("".join(lines), encoding="utf-8", newline="\n")
    return checksum_file


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("target", nargs="?", choices=TARGETS)
    parser.add_argument("--checksums", action="store_true", help="checksum all seven release archives")
    parser.add_argument("--root", type=Path, default=Path.cwd(), help="workspace root (default: cwd)")
    parser.add_argument("--output", type=Path, help="artifact directory (default: ROOT/artifacts)")
    args = parser.parse_args()
    if bool(args.target) == args.checksums:
        parser.error("provide either a target or --checksums")
    output = args.output if args.output is not None else args.root / "artifacts"
    try:
        result = write_checksums(output) if args.checksums else package_release(args.target, args.root, output)
    except (OSError, ValueError) as error:
        raise SystemExit(str(error)) from error
    print(result)


if __name__ == "__main__":
    main()
