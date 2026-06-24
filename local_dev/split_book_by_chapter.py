#!/usr/bin/env python3
"""Split a PDF into per-chapter PDFs using a chapter -> start-page index (JSON).

Reads a JSON map {"<chapter>": <start_page>, ...}, sorts the chapters
numerically, and for each chapter extracts pages [start, next_start - 1]. The
final chapter runs from its start page to the last page of the PDF. Each chapter
is written as "<chapter>.pdf".

PDF slicing is done with qpdf (already installed) -- no Python PDF libraries
needed. Python only parses the JSON, computes page ranges, and drives qpdf.

Run with no arguments to split the bundled book, or override any path:

    python3 split_book_by_chapter.py
    python3 split_book_by_chapter.py --dry-run
    python3 split_book_by_chapter.py --pdf other.pdf --index other.json --out ./chapters
"""
from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
BOOKS = HERE.parent / "my-first-project" / "books"
DEFAULT_PDF = BOOKS / "Yes!, 50 Scientifically Proven Ways to Be Persuasive.pdf"
DEFAULT_INDEX = BOOKS / "yes_!_page_index.json"
DEFAULT_OUT = BOOKS / "chapters"


def run(cmd: list[str]) -> str:
    """Run a command (list form, no shell) and return stdout; raise on failure."""
    result = subprocess.run(cmd, capture_output=True, text=True)
    if result.returncode != 0:
        raise RuntimeError(
            f"command failed (exit {result.returncode}): {' '.join(cmd)}\n"
            f"{result.stderr.strip()}"
        )
    return result.stdout.strip()


def page_count(pdf: Path) -> int:
    """Total number of pages in the PDF, via qpdf."""
    return int(run(["qpdf", "--show-npages", str(pdf)]))


def parse_args() -> argparse.Namespace:
    ap = argparse.ArgumentParser(
        description="Split a PDF into one PDF per chapter using a JSON page index."
    )
    ap.add_argument("--pdf", type=Path, default=DEFAULT_PDF, help="Source PDF.")
    ap.add_argument("--index", type=Path, default=DEFAULT_INDEX,
                    help="JSON file mapping chapter -> start page.")
    ap.add_argument("--out", type=Path, default=DEFAULT_OUT,
                    help="Output directory for the per-chapter PDFs.")
    ap.add_argument("--zero-based", action="store_true",
                    help="Treat the JSON start values as 0-based indices "
                         "(default: 1-based page numbers).")
    ap.add_argument("--pad", action="store_true",
                    help="Zero-pad output names (01.pdf .. 50.pdf) for sorting.")
    ap.add_argument("--dry-run", action="store_true",
                    help="Print the planned page ranges without writing files.")
    return ap.parse_args()


def main() -> int:
    args = parse_args()

    for path in (args.pdf, args.index):
        if not path.exists():
            print(f"ERROR: file not found: {path}", file=sys.stderr)
            return 1

    index = json.loads(args.index.read_text())
    if not index:
        print(f"ERROR: index is empty: {args.index}", file=sys.stderr)
        return 1

    # Sort chapters numerically (JSON keys are strings).
    chapters = sorted(index.items(), key=lambda kv: int(kv[0]))
    # Normalize start pages to 1-based (qpdf page ranges are 1-based).
    starts = [int(v) + (1 if args.zero_based else 0) for _, v in chapters]

    total = page_count(args.pdf)
    pad_width = len(str(max(int(k) for k, _ in chapters))) if args.pad else 0

    print(f"PDF:    {args.pdf.name} ({total} pages)")
    print(f"Index:  {args.index.name} ({len(chapters)} chapters)")
    print(f"Output: {args.out}{'  [dry-run]' if args.dry_run else ''}\n")

    if not args.dry_run:
        args.out.mkdir(parents=True, exist_ok=True)

    written = skipped = 0
    for i, (chapter, _) in enumerate(chapters):
        start = starts[i]
        end = (starts[i + 1] - 1) if i + 1 < len(starts) else total
        name = (chapter.zfill(pad_width) if args.pad else chapter) + ".pdf"
        dest = args.out / name

        if not (1 <= start <= end <= total):
            print(f"  ch {chapter:>3}: pages {start:>3}-{end:<4} -> {name:<10} "
                  f"SKIP (invalid range; total pages = {total})")
            skipped += 1
            continue

        print(f"  ch {chapter:>3}: pages {start:>3}-{end:<4} "
              f"({end - start + 1:>2}p) -> {name}")
        if not args.dry_run:
            run(["qpdf", str(args.pdf), "--pages", str(args.pdf),
                 f"{start}-{end}", "--", str(dest)])
            written += 1

    if args.dry_run:
        print(f"\nDry run: {len(chapters)} chapters planned, {skipped} would be skipped.")
    else:
        print(f"\nDone. {written} written, {skipped} skipped -> {args.out}")
    return 1 if skipped else 0


if __name__ == "__main__":
    raise SystemExit(main())
