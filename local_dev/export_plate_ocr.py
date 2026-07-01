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

    # fast-plate-ocr >=2.x moved the API: ONNXPlateRecognizer -> LicensePlateRecognizer, and the ONNX +
    # a YAML config (carrying the alphabet + pad char) are fetched via inference.hub.download_model.
    try:
        from fast_plate_ocr import LicensePlateRecognizer
        from fast_plate_ocr.inference import hub
    except ImportError:
        die("Missing fast-plate-ocr. Install: pip install 'fast-plate-ocr[onnx]'")

    print(f"Resolving fast-plate-ocr hub model '{args.hub_model}' …")
    try:
        # Constructing the recognizer downloads + caches the model; reuse the cache for the onnx path.
        rec = LicensePlateRecognizer(args.hub_model, device="cpu")
        onnx_path, _cfg_path = hub.download_model(model_name=args.hub_model)
    except Exception as e:  # noqa: BLE001
        die(f"Could not resolve '{args.hub_model}': {e}\n"
            "  If this is an SSL cert error on a framework Python, retry with:\n"
            "    SSL_CERT_FILE=$(python -c 'import certifi; print(certifi.where())') "
            "python local_dev/export_plate_ocr.py")
    shutil.copyfile(onnx_path, out)

    # Ordered class->char map. The recognizer's alphabet places the pad char (config.pad_char) at the
    # LAST class index. ocr.rs treats class index == len(charset) as the blank/pad, so we OMIT the pad
    # char here (charset length = real classes; model classes = len+1). This is the load-bearing
    # alignment from finding 6: charset[i] MUST equal the model's class i for the non-pad classes.
    alphabet = getattr(rec.config, "alphabet", None)
    pad = getattr(rec.config, "pad_char", None)
    if not alphabet:
        die("Exported the ONNX but could not read the alphabet from rec.config — write "
            "models/lp_ocr_charset.json by hand (JSON array of 1-char strings in class order).")
    charset = [c for c in alphabet if c != pad] if pad else list(alphabet)
    charset_out.write_text(json.dumps(charset, ensure_ascii=False))
    print(f"Wrote charset ({len(charset)} classes; pad {pad!r} at model index "
          f"{alphabet.index(pad) if pad and pad in alphabet else 'n/a'} omitted) -> {charset_out}")

    # fast-plate-ocr CCT is a FIXED-LENGTH per-slot head (max_plate_slots), NOT CTC — the worker's
    # default PLATE_OCR_CTC=false matches it (a CRNN/PaddleOCR recognizer would need PLATE_OCR_CTC=true).
    slots = getattr(rec.config, "max_plate_slots", None)
    print(f"Model head: fixed-length, max_plate_slots={slots} → keep PLATE_OCR_CTC=false (default).")
    print(f"Wrote {out}. sha256: {sha256(out)}")
    print("Validate: cargo test -p hushai-worker --test vision_pipeline inspect_plate_model_io_shapes -- --nocapture")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
