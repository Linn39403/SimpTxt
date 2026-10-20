#!/usr/bin/env python3
"""Generate deterministic, exact-size UTF-8 benchmark files and SHA-256 hashes.

Examples:
    python3 scripts/generate_fixtures.py --families source --sizes 10000000
    python3 scripts/generate_fixtures.py --families long --sizes 1000000000

Defaults generate all three families at decimal 10 MB, 100 MB, and 1 GB and need
3.33 GB of disk. Select one family/size when space is limited. Memory is bounded
by a 256 KiB output block. Files are never overwritten without --overwrite.
Generation and hash calculation are not editor benchmarks and do not reset
filesystem caches. A separate recorded cold/warm benchmark protocol is required.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import tempfile
import time


BUFFER_BYTES = 256 * 1024
# Keep these seeds identical to tests/large_file.rs.
SEEDS = {
    "source": (
        "/* source fixture: a multiline comment\n"
        "second comment line: café λ 世界 🙂 */\r\n"
        "fn example() {\n"
        '    let greeting = "hello, λ 世界 🙂";\r\n'
        '    let multiline = "first line\n'
        'second line";\n'
        '    println!("{greeting}: {multiline}");\r\n'
        "}\n"
    ).encode("utf-8"),
    "logs": (
        "2026-01-01T00:00:00Z INFO request=42 path=/home status=200\r\n"
        "2026-01-01T00:00:01Z INFO user=café message=世界🙂 duration_ms=17\n"
        "2026-01-01T00:00:02Z WARN retry=1 reason=λ_timeout\r\n"
    ).encode("utf-8"),
    "long": "0123456789abcdef café λ 世界 🙂 single_line_no_wrap ".encode("utf-8"),
}
PREFIXES = {"source": b"\xef\xbb\xbf", "logs": b"", "long": b""}
EXTENSIONS = {"source": "rs", "logs": "log", "long": "txt"}


def positive_size(value: str) -> int:
    size = int(value)
    if size < 3:
        raise argparse.ArgumentTypeError("sizes must be at least 3 bytes")
    return size


def fixture_chunks(family: str, size: int):
    """Yield bounded chunks whose concatenation is exactly size valid UTF-8 bytes.

    Individual output chunks can split a codepoint; the complete file cannot.
    An incomplete final codepoint is replaced by the same number of ASCII spaces.
    """
    prefix = PREFIXES[family]
    seed = SEEDS[family]
    if size < len(prefix):
        raise ValueError("size is smaller than the UTF-8 BOM")
    if prefix:
        yield prefix
    remaining = size - len(prefix)
    block = seed * max(1, BUFFER_BYTES // len(seed))
    while remaining >= len(block):
        yield block
        remaining -= len(block)
    if remaining:
        tail = (seed * ((remaining + len(seed) - 1) // len(seed)))[:remaining]
        try:
            tail.decode("utf-8")
        except UnicodeDecodeError as error:
            if error.reason != "unexpected end of data":
                raise
            tail = tail[: error.start] + b" " * (len(tail) - error.start)
        yield tail


def generate(destination: Path, family: str, size: int, overwrite: bool) -> dict:
    """Publish only a completely written file, then report its streaming hash."""
    if destination.exists() and not overwrite:
        raise FileExistsError(f"{destination} already exists; use --overwrite explicitly")
    started = time.perf_counter_ns()
    digest = hashlib.sha256()
    count = 0
    temporary_path = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="wb", prefix=".fixture-", dir=destination.parent, delete=False
        ) as output:
            temporary_path = Path(output.name)
            for chunk in fixture_chunks(family, size):
                output.write(chunk)
                digest.update(chunk)
                count += len(chunk)
            output.flush()
            os.fsync(output.fileno())
        if count != size or temporary_path.stat().st_size != size:
            raise RuntimeError(f"fixture length mismatch: expected {size}, wrote {count}")
        if overwrite:
            os.replace(temporary_path, destination)
        else:
            # Atomic create-new publication: do not clobber a racing creator.
            os.link(temporary_path, destination)
            temporary_path.unlink()
        temporary_path = None
    finally:
        if temporary_path is not None:
            temporary_path.unlink(missing_ok=True)
    return {
        "family": family,
        "path": str(destination),
        "bytes": count,
        "sha256": digest.hexdigest(),
        "generation_elapsed_ns": time.perf_counter_ns() - started,
        "encoding": "UTF-8",
        "bom": family == "source",
        "single_line": family == "long",
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--output-dir", type=Path, default=Path("benchmark-artifacts/fixtures")
    )
    parser.add_argument(
        "--families",
        nargs="+",
        choices=tuple(SEEDS),
        default=list(SEEDS),
        help="one or more fixture families (default: source logs long)",
    )
    parser.add_argument(
        "--sizes",
        type=positive_size,
        nargs="+",
        default=[10_000_000, 100_000_000, 1_000_000_000],
        help="exact decimal byte counts",
    )
    parser.add_argument("--overwrite", action="store_true")
    args = parser.parse_args()
    args.output_dir.mkdir(parents=True, exist_ok=True)
    records = []
    for family in dict.fromkeys(args.families):
        for size in dict.fromkeys(args.sizes):
            filename = f"{family}-{size}.{EXTENSIONS[family]}"
            record = generate(args.output_dir / filename, family, size, args.overwrite)
            records.append(record)
            # One flushed JSON record per file preserves partial-run evidence.
            print(json.dumps(record, sort_keys=True), flush=True)
    manifest = args.output_dir / "manifest.json"
    # Keep earlier independently generated families/sizes in the manifest.
    existing = []
    if manifest.exists():
        with manifest.open(encoding="utf-8") as source:
            existing = json.load(source)["fixtures"]
    by_path = {record["path"]: record for record in existing}
    by_path.update({record["path"]: record for record in records})
    payload = {
        "generator": "scripts/generate_fixtures.py",
        "version": 1,
        "byte_units": "decimal",
        "cache_state": "not controlled; generation populates filesystem caches",
        "fixtures": sorted(by_path.values(), key=lambda record: record["path"]),
    }
    with tempfile.NamedTemporaryFile(
        mode="w", encoding="utf-8", prefix=".manifest-", dir=args.output_dir, delete=False
    ) as output:
        temporary_manifest = Path(output.name)
        try:
            json.dump(payload, output, indent=2, sort_keys=True)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        except BaseException:
            temporary_manifest.unlink(missing_ok=True)
            raise
    try:
        os.replace(temporary_manifest, manifest)
    finally:
        temporary_manifest.unlink(missing_ok=True)


if __name__ == "__main__":
    main()
