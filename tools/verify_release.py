#!/usr/bin/env python3
"""Exercise a release package offline with Python 3.12+ and a local HTTP request."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import tarfile
import tempfile
import time
from urllib.error import URLError
from urllib.request import ProxyHandler, build_opener
import zipfile


def verify(archive: Path) -> None:
    archive = archive.resolve(strict=True)
    checksum = Path(str(archive) + ".sha256").read_text(encoding="utf-8").split()
    with archive.open("rb") as archive_file:
        actual = hashlib.file_digest(archive_file, "sha256").hexdigest()
    if checksum != [actual, archive.name]:
        raise ValueError("Archive checksum does not match its checksum file")

    with tempfile.TemporaryDirectory(prefix="pace-forge-release-") as temporary:
        # Windows runners can expose TEMP through an 8.3 alias (RUNNER~1).
        # Compare canonical paths on both sides of the archive boundary check.
        root = Path(temporary).resolve()
        extracted = root / "extracted"
        extracted.mkdir()
        if archive.name.endswith(".zip"):
            with zipfile.ZipFile(archive) as bundle:
                for member in bundle.infolist():
                    if not (extracted / member.filename).resolve().is_relative_to(extracted):
                        raise ValueError("Archive contains a path outside its root")
                bundle.extractall(extracted)
            executable_name = "pace-forge.exe"
            launcher_name = "start-web.cmd"
        elif archive.name.endswith(".tar.gz"):
            with tarfile.open(archive) as bundle:
                bundle.extractall(extracted, filter="data")
            executable_name = "pace-forge"
            launcher_name = "start-web.sh"
        else:
            raise ValueError("Expected a .zip or .tar.gz release archive")

        directories = list(extracted.iterdir())
        if len(directories) != 1 or not directories[0].is_dir():
            raise ValueError("Expected one top-level package directory")
        package = directories[0]
        expected = {executable_name, launcher_name, "README.md", "LICENSE", "build-info.json"}
        if {path.name for path in package.iterdir()} != expected:
            raise ValueError("Package file list does not match the release manifest")
        executable = package / executable_name
        if os.name != "nt" and not os.access(executable, os.X_OK):
            raise ValueError("Packaged binary lost its executable permission")
        if os.name != "nt" and not os.access(package / launcher_name, os.X_OK):
            raise ValueError("Packaged launcher lost its executable permission")

        # Only child processes receive this environment; the user's shell is unchanged.
        environment = os.environ.copy()
        environment["HOME"] = str(root / "home")
        environment["USERPROFILE"] = str(root / "home")
        environment.pop("PACE_FORGE_TOKEN", None)
        version = subprocess.run(
            [str(executable), "version"],
            check=True, capture_output=True, text=True, timeout=15,
            cwd=package, env=environment,
        ).stdout.strip()
        info = json.loads((package / "build-info.json").read_text(encoding="utf-8"))
        if version != f"pace-forge {info['version']}":
            raise ValueError("Executable version differs from build-info.json")
        for command in (["help"], ["check", "2", "12"], ["stadium", "5", "2"]):
            result = subprocess.run(
                [str(executable), *command],
                check=True, capture_output=True, timeout=30,
                cwd=package, env=environment,
            )
            if not result.stdout:
                raise ValueError(f"No output from {command[0]}")
            if command[0] == "check" and "判定: ✓ 通过".encode() not in result.stdout:
                raise ValueError("Packaged binary did not pass the offline track check")

        with socket.socket() as reserved:
            reserved.bind(("127.0.0.1", 0))
            port = reserved.getsockname()[1]
        with tempfile.TemporaryFile() as server_log:
            server = subprocess.Popen(
                [str(executable), "web", f"127.0.0.1:{port}"],
                cwd=package, env=environment,
                stdout=server_log, stderr=subprocess.STDOUT,
            )
            try:
                opener = build_opener(ProxyHandler({}))
                deadline = time.monotonic() + 15
                while True:
                    if server.poll() is not None:
                        raise RuntimeError("Packaged web server exited before serving a page")
                    try:
                        with opener.open(f"http://127.0.0.1:{port}/", timeout=1) as response:
                            page = response.read()
                            if response.status != 200 or b"<html" not in page.lower():
                                raise ValueError("Packaged web page is missing or invalid")
                        break
                    except (URLError, TimeoutError):
                        if time.monotonic() >= deadline:
                            raise TimeoutError("Packaged web server did not start within 15 seconds")
                        time.sleep(0.1)
            finally:
                if server.poll() is None:
                    server.terminate()
                try:
                    server.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    server.kill()
                    server.wait(timeout=5)
        if (root / "home" / ".pace-forge" / "session.json").exists():
            raise ValueError("Offline smoke checks unexpectedly created a login session")
        print(f"Verified {archive.name}: checksum, contents, version, offline CLI, local web")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path)
    args = parser.parse_args()
    verify(args.archive)


if __name__ == "__main__":
    main()
