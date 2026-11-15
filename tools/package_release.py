#!/usr/bin/env python3
"""Build Windows/Linux release archives from an already compiled binary.

Requires Python 3.11 or newer and the Rust toolchain used for the build.
Only the explicitly listed release files enter an archive; local account data
and caches are never collected.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import tarfile
import tempfile
import tomllib
import zipfile


REPO_ROOT = Path(__file__).resolve().parents[1]
TARGETS = {
    "x86_64-pc-windows-msvc": ("windows-x86_64", "pace-forge.exe", "start-web.cmd"),
    "x86_64-unknown-linux-musl": ("linux-x86_64", "pace-forge", "start-web.sh"),
}
VERSION_PATTERN = re.compile(
    r"\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?"
)
REVISION_PATTERN = re.compile(r"(?:[0-9a-fA-F]{40}|[0-9a-fA-F]{64})")


def package_version(tag: str | None = None) -> str:
    with (REPO_ROOT / "Cargo.toml").open("rb") as manifest:
        package = tomllib.load(manifest)["package"]
    version = package.get("version")
    if not isinstance(version, str) or not VERSION_PATTERN.fullmatch(version):
        raise ValueError("Cargo.toml must contain a valid package version")
    if tag is not None and tag != f"v{version}":
        raise ValueError(f"release tag {tag!r} must match Cargo.toml version v{version}")
    return version


def tool_version(tool: str) -> str:
    try:
        result = subprocess.run(
            [tool, "--version"],
            check=True,
            capture_output=True,
            text=True,
            encoding="utf-8",
        )
    except (OSError, subprocess.CalledProcessError) as error:
        raise ValueError(f"could not read {tool} version; ensure Rust is on PATH") from error
    return result.stdout.strip()


def release_files(target: str, version: str, revision: str) -> dict[str, tuple[bytes, int]]:
    _, binary_name, launcher_name = TARGETS[target]
    inputs = {
        binary_name: (REPO_ROOT / "target" / target / "release" / binary_name, 0o755),
        "README.md": (REPO_ROOT / "README.md", 0o644),
        "LICENSE": (REPO_ROOT / "LICENSE", 0o644),
        launcher_name: (REPO_ROOT / "tools" / "release" / launcher_name, 0o755),
    }
    files: dict[str, tuple[bytes, int]] = {}
    for name, (path, mode) in inputs.items():
        if not path.is_file():
            raise ValueError(f"missing release input: {path}")
        data = path.read_bytes()
        if name == "start-web.cmd":
            data = data.replace(b"\r\n", b"\n").replace(b"\n", b"\r\n")
        files[name] = (data, mode)
    build_info = {
        "version": version,
        "revision": revision,
        "target": target,
        "rustc": tool_version("rustc"),
        "cargo": tool_version("cargo"),
    }
    files["build-info.json"] = (
        (json.dumps(build_info, indent=2, sort_keys=True) + "\n").encode("utf-8"),
        0o644,
    )
    return files


def write_zip(path: Path, root: str, files: dict[str, tuple[bytes, int]]) -> None:
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
        directory = zipfile.ZipInfo(f"{root}/", date_time=(1980, 1, 1, 0, 0, 0))
        directory.create_system = 3
        directory.external_attr = ((stat.S_IFDIR | 0o755) << 16) | 0x10
        archive.writestr(directory, b"")
        for name, (data, mode) in sorted(files.items()):
            entry = zipfile.ZipInfo(f"{root}/{name}", date_time=(1980, 1, 1, 0, 0, 0))
            entry.create_system = 3
            entry.compress_type = zipfile.ZIP_DEFLATED
            entry.external_attr = (stat.S_IFREG | mode) << 16
            archive.writestr(entry, data, compresslevel=9)


def write_tar(path: Path, root: str, files: dict[str, tuple[bytes, int]]) -> None:
    with path.open("wb") as output:
        with gzip.GzipFile(filename="", mode="wb", fileobj=output, mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as archive:
                directory = tarfile.TarInfo(root)
                directory.type = tarfile.DIRTYPE
                directory.mode = 0o755
                archive.addfile(directory)
                for name, (data, mode) in sorted(files.items()):
                    entry = tarfile.TarInfo(f"{root}/{name}")
                    entry.size = len(data)
                    entry.mode = mode
                    archive.addfile(entry, io.BytesIO(data))


def make_package(args: argparse.Namespace) -> tuple[Path, Path]:
    version = package_version(args.tag)
    if not REVISION_PATTERN.fullmatch(args.revision):
        raise ValueError("revision must be a complete 40- or 64-character hexadecimal commit SHA")
    revision = args.revision.lower()
    platform, _, _ = TARGETS[args.target]
    label = f"v{version}" if args.tag is not None else f"v{version}-{revision[:7]}"
    stem = f"pace-forge-{label}-{platform}"
    suffix = ".zip" if platform.startswith("windows-") else ".tar.gz"
    files = release_files(args.target, version, revision)
    output_dir = Path(args.output_dir).resolve()
    output_dir.mkdir(parents=True, exist_ok=True)
    archive_path = output_dir / f"{stem}{suffix}"
    checksum_path = output_dir / f"{archive_path.name}.sha256"
    with tempfile.NamedTemporaryFile(
        prefix=f".{stem}-", suffix=".tmp", dir=output_dir, delete=False
    ) as temporary:
        temporary_path = Path(temporary.name)
    try:
        if suffix == ".zip":
            write_zip(temporary_path, stem, files)
        else:
            write_tar(temporary_path, stem, files)
        digest = hashlib.sha256(temporary_path.read_bytes()).hexdigest()
        os.replace(temporary_path, archive_path)
        checksum_path.write_text(f"{digest}  {archive_path.name}\n", encoding="utf-8", newline="\n")
    finally:
        temporary_path.unlink(missing_ok=True)
    return archive_path, checksum_path


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    version_parser = commands.add_parser("version", help="print Cargo.toml's package version")
    version_parser.add_argument("--tag", help="require a release tag matching v<version>")
    package_parser = commands.add_parser("package", help="archive one supported target's binary")
    package_parser.add_argument("--target", choices=TARGETS, required=True)
    package_parser.add_argument("--revision", required=True, help="complete source commit SHA")
    package_parser.add_argument("--tag", help="release tag; omitted for commit-labelled previews")
    package_parser.add_argument("--output-dir", default="dist")
    args = parser.parse_args()
    try:
        if args.command == "version":
            print(package_version(args.tag))
        else:
            archive_path, checksum_path = make_package(args)
            print(archive_path)
            print(checksum_path)
    except (OSError, ValueError, KeyError) as error:
        parser.exit(1, f"error: {error}\n")


if __name__ == "__main__":
    main()
