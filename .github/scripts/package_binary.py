#!/usr/bin/env python3
"""Package a local release binary with deterministic archive metadata; never publish."""
import gzip
import hashlib
import io
import platform
import tarfile
from pathlib import Path

root = Path(__file__).resolve().parents[2]
dist = root / "dist"
dist.mkdir(exist_ok=True)
archive = dist / f"ledger-{platform.system().lower()}-{platform.machine()}.tar.gz"
sources = [
    (root / "target/release/ledger", "ledger", 0o755),
    (root / "README.md", "README.md", 0o644),
    (root / "ARCHITECTURE.md", "ARCHITECTURE.md", 0o644),
    (root / "Cargo.lock", "Cargo.lock", 0o644),
    (dist / "build-info.txt", "build-info.txt", 0o644),
]
with archive.open("wb") as output:
    with gzip.GzipFile(filename="", mode="wb", fileobj=output, mtime=0) as compressed:
        with tarfile.open(fileobj=compressed, mode="w|") as package:
            for path, name, mode in sources:
                data = path.read_bytes()
                info = tarfile.TarInfo(name)
                info.size, info.mode, info.mtime = len(data), mode, 0
                package.addfile(info, io.BytesIO(data))
digest = hashlib.sha256(archive.read_bytes()).hexdigest()
archive.with_suffix(archive.suffix + ".sha256").write_text(
    f"{digest}  {archive.name}\n", encoding="utf-8"
)
print(archive)
