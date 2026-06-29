#!/usr/bin/env python3
"""Export a license-plate OCR model (fast-plate-ocr CCT, default) to ONNX + its charset sidecar for
the worker's ALPR lane (hushai-worker/src/vision/plates/ocr.rs). NOT tesseract.

WHY (see AGENTS.md "vision"): after a plate is detected, rectified (deskewed), super-resolved and
contrast-normalized, ocr.rs reads it. The recognizer is a defensive decoder: it inspects the graph's
declared I/O to pick the layout (NCHW vs NHWC, grayscale vs RGB), runs greedy per-timestep argmax
with CTC blank/duplicate collapse, and maps class indices through the charset written here. Validate:
    cargo test -p hushai-worker --test vision_pipeline inspect_plate_model_io_shapes -- --nocapture

Output (gitignored under models/):
    models/lp_ocr_cct.onnx        default PLATE_OCR_MODEL_PATH
    models/lp_ocr_charset.json    the load-bearing ordered class->char map (array of 1-char strings),
                                  default PLATE_OCR_CHARSET_PATH. ocr.rs maps class index i -> charset[i];
                                  a CTC blank is assumed at index len(charset) when classes == len+1.

LICENSE: fast-plate-ocr is MIT (a clean choice for a shipped product); weights gitignored under models/.

Usage:
    python3 local_dev/export_plate_ocr.py                 # fast-plate-ocr CCT -> models/lp_ocr_cct.onnx
    python3 local_dev/export_plate_ocr.py --hub-model cct-s-v1
    python3 local_dev/export_plate_ocr.py --force
"""
from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
MODELS_DIR = REPO_ROOT / "models"


def die(msg: str, code: int = 1):
    print(msg, file=sys.stderr)
    sys.exit(code)


def sha256(p: Path) -> str:
    h = hashlib.sha256()
    with p.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--hub-model", default="cct-xs-v1-global-model",
                    help="fast-plate-ocr hub model name (default cct-xs-v1-global-model)")
    ap.add_argument("--force", action="store_true")
    args = ap.parse_args()

    out = MODELS_DIR / "lp_ocr_cct.onnx"
    charset_out = MODELS_DIR / "lp_ocr_charset.json"
    if out.exists() and charset_out.exists() and not args.force:
        print(f"{out} + charset already exist (use --force). sha256: {sha256(out)}")
        return 0
    MODELS_DIR.mkdir(parents=True, exist_ok=True)

    try:
        from fast_plate_ocr import ONNXPlateRecognizer
    except ImportError:
        die("Missing fast-plate-ocr. Install: pip install fast-plate-ocr")

    # fast-plate-ocr downloads the ONNX + a config (which carries the alphabet) to a local cache.
    print(f"Resolving fast-plate-ocr hub model '{args.hub_model}' …")
    rec = ONNXPlateRecognizer(args.hub_model)

    # Locate the cached .onnx + its config; APIs differ across versions, so probe defensively.
    onnx_path = None
    alphabet = None
    for attr in ("model_path", "_model_path", "onnx_model_path"):
        p = getattr(rec, attr, None)
        if p and Path(p).exists():
            onnx_path = Path(p)
            break
    cfg = getattr(rec, "config", None) or getattr(rec, "_config", None)
    if cfg is not None:
        alphabet = cfg.get("alphabet") if isinstance(cfg, dict) else getattr(cfg, "alphabet", None)

    if onnx_path is None:
        die("Could not locate the cached ONNX from fast-plate-ocr; inspect the installed version's API "
            "(the recognizer object should expose the model path + config alphabet) and adapt this script.")
    shutil.copyfile(onnx_path, out)

    if alphabet:
        # Each class is one char; the alphabet string's order IS the class order.
        charset = list(alphabet)
        charset_out.write_text(json.dumps(charset, ensure_ascii=False))
        print(f"Wrote charset ({len(charset)} classes) -> {charset_out}")
    else:
        die("Exported the ONNX but could not read the alphabet — write models/lp_ocr_charset.json by "
            "hand (a JSON array of single-char strings in class-index order) before running the worker.")

    print(f"Wrote {out}. sha256: {sha256(out)}")
    print("Validate: cargo test -p hushai-worker --test vision_pipeline inspect_plate_model_io_shapes -- --nocapture")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
