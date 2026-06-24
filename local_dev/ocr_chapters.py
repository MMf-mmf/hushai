#!/usr/bin/env python3
"""OCR each chapter PDF into a plain-text file using Tesseract.

The book is a scanned/image-only PDF, so the per-chapter PDFs have no text
layer. For each chapter PDF this script:

  1. renders every page to a grayscale image with `pdftoppm` (poppler), and
  2. runs `tesseract` on each page image, concatenating the text.

The result is written as "<chapter>.txt". Like split_book_by_chapter.py, this
only orchestrates already-installed CLI tools (pdftoppm + tesseract) -- no
Python PDF/OCR packages required.

    python3 ocr_chapters.py                  # OCR every chapter
    python3 ocr_chapters.py --chapters 1 2   # only chapters 1 and 2
    python3 ocr_chapters.py --force          # redo chapters already OCR'd
"""
from __future__ import annotations

import argparse
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
BOOKS = HERE.parent / "my-first-project" / "books"
DEFAULT_INPUT = BOOKS / "chapters"
DEFAULT_OUT = BOOKS / "chapters_text"


def render_pages(pdf: Path, tmp: Path, dpi: int) -> list[Path]:
    """Render every page of `pdf` to a grayscale PNG in `tmp`, returned in order."""
    prefix = tmp / "pg"
    result = subprocess.run(
        ["pdftoppm", "-r", str(dpi), "-gray", "-png", str(pdf), str(prefix)],
        capture_output=True, text=True, errors="replace",
    )
    if result.returncode != 0:
        raise RuntimeError(f"pdftoppm failed on {pdf.name}: {result.stderr.strip()}")
    return sorted(tmp.glob("pg-*.png"), key=lambda p: int(p.stem.rsplit("-", 1)[-1]))


def ocr_image(png_bytes: bytes, lang: str, psm: int) -> str:
    """OCR a single PNG, fed via stdin.

    tesseract's own file-path open is unreliable in this environment, but reading
    the image bytes from stdin (`tesseract - stdout`) works correctly, so we read
    each PNG in Python and pipe it in.
    """
    result = subprocess.run(
        ["tesseract", "-", "stdout", "-l", lang, "--psm", str(psm)],
        input=png_bytes, capture_output=True,
    )
    if result.returncode != 0:
        raise RuntimeError(
            "tesseract failed: " + result.stderr.decode("utf-8", "replace").strip()
        )
    return result.stdout.decode("utf-8", "replace").strip()


def ocr_pdf(pdf: Path, dpi: int, lang: str, psm: int) -> tuple[str, int]:
    """Render every page of `pdf` and OCR each. Returns (text, n_pages)."""
    with tempfile.TemporaryDirectory() as tmp:
        pages = render_pages(pdf, Path(tmp), dpi)
        texts = [ocr_image(p.read_bytes(), lang, psm) for p in pages]
    return "\n\n".join(texts).strip() + "\n", len(pages)


def chapter_key(pdf: Path) -> int | None:
    """Numeric chapter from a filename like '7.pdf'; None if not numeric."""
    return int(pdf.stem) if pdf.stem.isdigit() else None


def parse_args() -> argparse.Namespace:
    ap = argparse.ArgumentParser(description="OCR each chapter PDF into a .txt file.")
    ap.add_argument("--input", type=Path, default=DEFAULT_INPUT,
                    help="Directory of per-chapter PDFs (default: books/chapters).")
    ap.add_argument("--out", type=Path, default=DEFAULT_OUT,
                    help="Directory for the .txt output (default: books/chapters_text).")
    ap.add_argument("--chapters", type=int, nargs="+", metavar="N",
                    help="Only OCR these chapter numbers (default: all).")
    ap.add_argument("--dpi", type=int, default=300, help="Render resolution (default: 300).")
    ap.add_argument("--lang", default="eng", help="Tesseract language (default: eng).")
    ap.add_argument("--psm", type=int, default=3,
                    help="Tesseract page-segmentation mode (default: 3 = auto).")
    ap.add_argument("--force", action="store_true",
                    help="Re-OCR chapters whose .txt already exists.")
    return ap.parse_args()


def main() -> int:
    args = parse_args()

    if not args.input.is_dir():
        print(f"ERROR: input directory not found: {args.input}", file=sys.stderr)
        return 1

    pdfs = sorted((p for p in args.input.glob("*.pdf") if chapter_key(p) is not None),
                  key=chapter_key)
    if args.chapters:
        wanted = set(args.chapters)
        pdfs = [p for p in pdfs if chapter_key(p) in wanted]
    if not pdfs:
        print(f"ERROR: no matching chapter PDFs in {args.input}", file=sys.stderr)
        return 1

    args.out.mkdir(parents=True, exist_ok=True)
    print(f"Input:  {args.input}")
    print(f"Output: {args.out}")
    print(f"OCR:    tesseract -l {args.lang} --psm {args.psm} @ {args.dpi} dpi "
          f"({len(pdfs)} chapters)\n")

    done = skipped = failed = 0
    for pdf in pdfs:
        dest = args.out / (pdf.stem + ".txt")
        if dest.exists() and not args.force:
            print(f"  ch {pdf.stem:>3}: skip (exists, use --force to redo)")
            skipped += 1
            continue
        try:
            text, n_pages = ocr_pdf(pdf, args.dpi, args.lang, args.psm)
        except Exception as exc:  # one bad chapter shouldn't abort the whole batch
            print(f"  ch {pdf.stem:>3}: FAILED ({exc})")
            failed += 1
            continue
        dest.write_text(text, encoding="utf-8")
        words = len(text.split())
        print(f"  ch {pdf.stem:>3}: {n_pages:>2} pages -> {dest.name:<9} "
              f"({words:>4} words)")
        done += 1

    print(f"\nDone. {done} OCR'd, {skipped} skipped, {failed} failed -> {args.out}")
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
