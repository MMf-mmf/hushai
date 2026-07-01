#!/usr/bin/env python3
"""Export RF-DETR-Nano to ONNX for the worker's open-vocabulary object lane (Phase B).

WHY (see AGENTS.md "vision"): the worker's object detector (hushai-worker/src/vision/objects.rs)
dlopen()s a 1.20 ONNX Runtime and runs an RF-DETR graph whose I/O contract was, until now,
*defensive and UNVALIDATED at rest*. This script produces the real graph so that
`tests/vision_pipeline.rs::inspect_object_model_io_shapes` + `detect_objects_from_real_video`
can validate (and, if needed, correct) the decode in objects.rs.

RF-DETR (Roboflow, Apache-2.0) is COCO-trained. The Nano variant uses a 384x384 square input,
which matches the worker default `OBJECT_DET_INPUT_SIZE=384` (config.rs). The ONNX export emits two
outputs read BY ORDER in objects.rs:
    dets   : [1, N, 4]   bounding boxes, **cxcywh, normalized [0,1]**
    labels : [1, N, C]   class logits (sigmoid-activated; RF-DETR uses focal/sigmoid heads)

CLASS INDEXING IS THE LOAD-BEARING VALIDATION POINT. RF-DETR's COCO head is NOT a dense 80-class
space — it carries the historical 90/91-slot COCO category layout (with gaps + a background slot).
objects.rs currently maps with a dense COCO-80 table, which will mislabel unless corrected. So this
script ALSO writes `models/rf-detr-classes.json` (the authoritative index→name map from the rfdetr
package) and prints C, so the Rust `coco_label` can be aligned to ground truth.

Output (gitignored under models/):
    models/rf-detr-nano.onnx       the exported graph (default OBJECT_DET_MODEL_PATH)
    models/rf-detr-classes.json    {index: "name"} the model's real class layout

Usage:
    python3 local_dev/export_rf_detr.py                 # nano @384 -> models/rf-detr-nano.onnx
    python3 local_dev/export_rf_detr.py --variant small --resolution 512
    python3 local_dev/export_rf_detr.py --force         # re-export even if the file exists

Run once after a fresh checkout; fully offline at runtime thereafter. Requires `pip install rfdetr
onnx` (the script prints the exact command if a dependency is missing).
"""
from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
MODELS_DIR = REPO_ROOT / "models"


def die(msg: str, code: int = 1) -> None:
    print(f"ERROR: {msg}", file=sys.stderr)
    sys.exit(code)


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def main() -> None:
    ap = argparse.ArgumentParser(description="Export RF-DETR to ONNX for the worker object lane.")
    ap.add_argument(
        "--variant",
        default="nano",
        choices=["nano", "small", "medium", "base", "large"],
        help="RF-DETR size (default nano; matches OBJECT_DET_MODEL_PATH=./models/rf-detr-nano.onnx).",
    )
    ap.add_argument(
        "--resolution",
        type=int,
        default=384,
        help="Square input side. MUST equal OBJECT_DET_INPUT_SIZE (worker default 384).",
    )
    ap.add_argument("--out", default=str(MODELS_DIR / "rf-detr-nano.onnx"), help="Output .onnx path.")
    ap.add_argument("--opset", type=int, default=17, help="ONNX opset (>=17).")
    ap.add_argument("--force", action="store_true", help="Re-export even if the output exists.")
    args = ap.parse_args()

    out_path = Path(args.out)
    if out_path.exists() and not args.force:
        print(f"RF-DETR ONNX already present at {out_path} — nothing to do (use --force to re-export).")
        print(f"sha256: {sha256(out_path)}")
        return

    try:
        import rfdetr  # noqa: F401
        from rfdetr import (  # type: ignore
            RFDETRBase,
            RFDETRLarge,
            RFDETRMedium,
            RFDETRNano,
            RFDETRSmall,
        )
    except Exception as e:  # pragma: no cover - operator provisioning path
        die(
            "could not import `rfdetr` "
            f"({e}).\n  Install it with:  pip install rfdetr onnx onnxsim\n"
            "  (downloads the COCO-pretrained checkpoint from Roboflow on first construction)."
        )

    ctor = {
        "nano": RFDETRNano,
        "small": RFDETRSmall,
        "medium": RFDETRMedium,
        "base": RFDETRBase,
        "large": RFDETRLarge,
    }[args.variant]

    print(f"Building RF-DETR-{args.variant} @ {args.resolution}px (downloads COCO weights on first run)…")
    try:
        model = ctor(resolution=args.resolution)
    except TypeError:
        # Older rfdetr ctors don't accept resolution; fall back to default + warn.
        print("  (this rfdetr build ignores `resolution`; using its default input size)")
        model = ctor()

    MODELS_DIR.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as td:
        print("Exporting to ONNX…")
        # rfdetr writes <output_dir>/inference_model.onnx (name varies by version); glob for it.
        # Newer rfdetr.export() dropped the `simplify` kwarg — fall back without it.
        try:
            model.export(output_dir=td, opset_version=args.opset, simplify=True)
        except TypeError:
            model.export(output_dir=td, opset_version=args.opset)
        produced = sorted(Path(td).glob("*.onnx"))
        if not produced:
            die(f"rfdetr.export produced no .onnx in {td}")
        # Prefer an 'inference'-named graph if multiple were emitted.
        src = next((p for p in produced if "inference" in p.name), produced[0])
        shutil.move(str(src), str(out_path))
    print(f"Wrote {out_path}")

    # --- Introspect + dump the real I/O + class layout (ground truth for objects.rs) ---
    try:
        import onnx

        m = onnx.load(str(out_path))
        print("== ONNX graph I/O ==")
        for inp in m.graph.input:
            dims = [d.dim_value or d.dim_param or "?" for d in inp.type.tensor_type.shape.dim]
            print(f"  IN  {inp.name} : {dims}")
        n_classes = None
        for out in m.graph.output:
            dims = [d.dim_value or d.dim_param or "?" for d in out.type.tensor_type.shape.dim]
            print(f"  OUT {out.name} : {dims}")
            if len(dims) == 3 and isinstance(dims[2], int) and dims[2] not in (4,):
                n_classes = dims[2]
        print(
            "NOTE: objects.rs reads outputs BY ORDER: first=boxes[1,N,4] (cxcywh normalized), "
            "second=logits[1,N,C]. Confirm this order matches the two OUT lines above."
        )
    except Exception as e:  # pragma: no cover
        print(f"(skipping onnx introspection: {e}; pip install onnx to enable)")
        n_classes = None

    # Authoritative class map from the package, written next to the model.
    classes_path = MODELS_DIR / "rf-detr-classes.json"
    try:
        names = _coco_class_map(n_classes)
        classes_path.write_text(json.dumps(names, indent=2))
        print(f"Wrote {classes_path} ({len(names)} classes). objects.rs `coco_label` must match THIS map.")
    except Exception as e:  # pragma: no cover
        print(f"(could not extract class names: {e}; validate labels via the inspect test instead)")

    print(f"sha256: {sha256(out_path)}")
    print(
        "\nNext: validate the decode against this real export:\n"
        "  cargo test -p hushai-worker --test vision_pipeline inspect_object_model_io_shapes -- --nocapture\n"
        "  OBJECT_TEST_MP4=./object_clip.mp4 cargo test -p hushai-worker --test vision_pipeline "
        "detect_objects_from_real_video -- --nocapture"
    )


def _coco_class_map(n_classes: int | None) -> dict:
    """Best-effort authoritative {index: name}. Tries the rfdetr package's own COCO names first."""
    # rfdetr ships COCO class names; the attribute path has moved across versions.
    for modpath, attr in [
        ("rfdetr.util.coco_classes", "COCO_CLASSES"),
        ("rfdetr.datasets.coco_classes", "COCO_CLASSES"),
    ]:
        try:
            mod = __import__(modpath, fromlist=[attr])
            obj = getattr(mod, attr)
            if isinstance(obj, dict):
                return {str(k): v for k, v in obj.items()}
            if isinstance(obj, (list, tuple)):
                return {str(i): v for i, v in enumerate(obj)}
        except Exception:
            continue
    # Fallback: the dense COCO-80 list (matches objects.rs today). If the model's C != 80, this is a
    # RED FLAG that objects.rs needs the 90/91-slot map — the printed C above tells you.
    coco80 = [
        "person", "bicycle", "car", "motorcycle", "airplane", "bus", "train", "truck", "boat",
        "traffic light", "fire hydrant", "stop sign", "parking meter", "bench", "bird", "cat",
        "dog", "horse", "sheep", "cow", "elephant", "bear", "zebra", "giraffe", "backpack",
        "umbrella", "handbag", "tie", "suitcase", "frisbee", "skis", "snowboard", "sports ball",
        "kite", "baseball bat", "baseball glove", "skateboard", "surfboard", "tennis racket",
        "bottle", "wine glass", "cup", "fork", "knife", "spoon", "bowl", "banana", "apple",
        "sandwich", "orange", "broccoli", "carrot", "hot dog", "pizza", "donut", "cake", "chair",
        "couch", "potted plant", "bed", "dining table", "toilet", "tv", "laptop", "mouse", "remote",
        "keyboard", "cell phone", "microwave", "oven", "toaster", "sink", "refrigerator", "book",
        "clock", "vase", "scissors", "teddy bear", "hair drier", "toothbrush",
    ]
    out = {str(i): n for i, n in enumerate(coco80)}
    if n_classes and n_classes != len(coco80):
        out["__warning__"] = (
            f"model has C={n_classes} classes but this fallback is dense COCO-80. "
            "objects.rs coco_label() likely needs the 90/91-slot RF-DETR map; "
            "install rfdetr's coco class names or inspect detections to build the correct table."
        )
    return out


if __name__ == "__main__":
    main()
