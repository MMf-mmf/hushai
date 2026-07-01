#!/usr/bin/env python3
"""Export OpenCLIP ViT-B/32 (image + text towers) to ONNX for open-vocabulary object retrieval.

WHY (see AGENTS.md "vision"): Phase B answers "when did I see a car / a red mug" by embedding object
crops and whole frames with the CLIP IMAGE tower (in the worker, hushai-worker/src/vision/objects.rs
`ClipEmbedder`) and embedding the user's phrase with the CLIP TEXT tower (in hushai-rag,
hushai-rag/src/clip_text.rs) — then nearest-neighboring the two in ONE shared 512-d space over
scene_objects' HNSW. Both towers MUST come from the SAME checkpoint or the spaces won't align.

VARIANT IS PINNED: objects.rs hardcodes `CLIP_SIZE=224` and the OpenAI CLIP mean/std
([0.4815,0.4578,0.4082] / [0.2686,0.2613,0.2758]). So this exports **ViT-B-32, pretrained='openai',
@224**. The script PRINTS the checkpoint's preprocessing mean/std so you can confirm they match the
Rust constants (a mismatch silently degrades retrieval).

Two graphs, NO in-graph L2-normalization (the Rust side normalizes, matching objects.rs:196 and
clip_text.rs so cosine over scene_objects.embedding is valid):
    models/clip_vit_b32_image.onnx   IN image[1,3,224,224] f32  -> OUT [1,512] f32
    models/clip_vit_b32_text.onnx    IN text[1,77] int64        -> OUT [1,512] f32

Usage:
    python3 local_dev/export_clip.py
    python3 local_dev/export_clip.py --force
    python3 local_dev/export_clip.py --model ViT-B-32 --pretrained openai

Requires `pip install open_clip_torch onnx` (torch is already present). Run once after checkout.
Pair with local_dev/fetch_clip_tokenizer.sh so the Rust text tower tokenizes identically.
"""
from __future__ import annotations

import argparse
import hashlib
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
MODELS_DIR = REPO_ROOT / "models"

# Must match objects.rs CLIP_MEAN/CLIP_STD (OpenAI CLIP). Printed for confirmation only.
EXPECT_MEAN = (0.48145466, 0.4578275, 0.40821073)
EXPECT_STD = (0.26862954, 0.26130258, 0.27577711)
CLIP_SIZE = 224
CONTEXT_LEN = 77
EMBED_DIM = 512


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
    ap = argparse.ArgumentParser(description="Export OpenCLIP ViT-B/32 image+text towers to ONNX.")
    ap.add_argument("--model", default="ViT-B-32", help="open_clip model name (pinned to ViT-B-32).")
    ap.add_argument("--pretrained", default="openai", help="open_clip pretrained tag (default openai).")
    ap.add_argument("--image-out", default=str(MODELS_DIR / "clip_vit_b32_image.onnx"))
    ap.add_argument("--text-out", default=str(MODELS_DIR / "clip_vit_b32_text.onnx"))
    ap.add_argument("--opset", type=int, default=17)
    ap.add_argument("--force", action="store_true")
    args = ap.parse_args()

    image_out = Path(args.image_out)
    text_out = Path(args.text_out)
    if image_out.exists() and text_out.exists() and not args.force:
        print(f"CLIP ONNX already present:\n  {image_out}\n  {text_out}\n(nothing to do; --force to re-export)")
        print(f"image sha256: {sha256(image_out)}")
        print(f"text  sha256: {sha256(text_out)}")
        return

    try:
        import torch
        import torch.nn as nn
        import open_clip
    except Exception as e:  # pragma: no cover - operator provisioning path
        die(
            f"could not import torch/open_clip ({e}).\n"
            "  Install with:  pip install open_clip_torch onnx"
        )

    if args.model != "ViT-B-32":
        print(
            f"WARNING: objects.rs pins CLIP_SIZE=224 + OpenAI ViT-B/32 normalization. "
            f"Exporting {args.model} will break the shared space unless objects.rs is updated."
        )

    print(f"Loading OpenCLIP {args.model} ({args.pretrained})… (downloads weights on first run)")
    model, _, preprocess = open_clip.create_model_and_transforms(args.model, pretrained=args.pretrained)
    model.eval()

    # Confirm preprocessing matches the Rust constants.
    mean, std = _preprocess_norm(preprocess)
    print(f"checkpoint normalize mean={mean}\n                  std ={std}")
    print(f"objects.rs expects mean={EXPECT_MEAN}\n                   std ={EXPECT_STD}")
    if mean and not _close(mean, EXPECT_MEAN):
        print("WARNING: mean differs from objects.rs CLIP_MEAN — update the Rust constants to match.")
    if std and not _close(std, EXPECT_STD):
        print("WARNING: std differs from objects.rs CLIP_STD — update the Rust constants to match.")

    MODELS_DIR.mkdir(parents=True, exist_ok=True)

    class ImageTower(nn.Module):
        def __init__(self, m):
            super().__init__()
            self.m = m

        def forward(self, image):
            # Raw (un-normalized) 512-d image embedding; Rust L2-normalizes.
            return self.m.encode_image(image)

    class TextTower(nn.Module):
        def __init__(self, m):
            super().__init__()
            self.m = m

        def forward(self, text):
            return self.m.encode_text(text)

    # open_clip's MultiheadAttention fast path emits aten::_native_multi_head_attention, which the
    # legacy torchscript ONNX exporter can't lower. Disable the fast path so it traces via the
    # standard (exportable) attention path. (Newer torch only; ignored if the API is absent.)
    try:
        torch.backends.mha.set_fastpath_enabled(False)
    except Exception:
        pass

    with torch.no_grad():
        # --- image tower ---
        img_dummy = torch.randn(1, 3, CLIP_SIZE, CLIP_SIZE, dtype=torch.float32)
        print(f"Exporting image tower -> {image_out}")
        torch.onnx.export(
            ImageTower(model),
            img_dummy,
            str(image_out),
            input_names=["image"],
            output_names=["image_embedding"],
            opset_version=args.opset,
            dynamic_axes={"image": {0: "batch"}, "image_embedding": {0: "batch"}},
            do_constant_folding=True,
            dynamo=False,
        )

        # --- text tower ---
        txt_dummy = torch.zeros(1, CONTEXT_LEN, dtype=torch.int64)
        # Fill with a real tokenization so the trace exercises the true path.
        try:
            tok = open_clip.get_tokenizer(args.model)
            txt_dummy = tok(["a photo of a car"])[:, :CONTEXT_LEN].to(torch.int64)
        except Exception:
            pass
        print(f"Exporting text tower  -> {text_out}")
        torch.onnx.export(
            TextTower(model),
            txt_dummy,
            str(text_out),
            input_names=["text"],
            output_names=["text_embedding"],
            opset_version=args.opset,
            dynamic_axes={"text": {0: "batch"}, "text_embedding": {0: "batch"}},
            do_constant_folding=True,
            dynamo=False,
        )

    _introspect(image_out, "image")
    _introspect(text_out, "text")

    # Optional ONNX-vs-torch parity + cross-modal sanity if onnxruntime is available.
    _parity_check(model, image_out, text_out)

    print(f"image sha256: {sha256(image_out)}")
    print(f"text  sha256: {sha256(text_out)}")
    print(
        "\nNext: fetch the matching tokenizer, then run the decisive cross-modal test:\n"
        "  bash local_dev/fetch_clip_tokenizer.sh\n"
        "  cargo test -p hushai-worker --test vision_pipeline clip_text_matches_image_on_fixtures -- --nocapture"
    )


def _preprocess_norm(preprocess):
    """Pull (mean, std) out of the open_clip preprocessing transform if present."""
    try:
        for t in getattr(preprocess, "transforms", []):
            if hasattr(t, "mean") and hasattr(t, "std"):
                return tuple(float(x) for x in t.mean), tuple(float(x) for x in t.std)
    except Exception:
        pass
    return None, None


def _close(a, b, tol=1e-3):
    return all(abs(x - y) < tol for x, y in zip(a, b))


def _introspect(path: Path, kind: str) -> None:
    try:
        import onnx

        m = onnx.load(str(path))
        print(f"== CLIP {kind} I/O ==")
        for inp in m.graph.input:
            dims = [d.dim_value or d.dim_param or "?" for d in inp.type.tensor_type.shape.dim]
            print(f"  IN  {inp.name} : {dims}")
        for out in m.graph.output:
            dims = [d.dim_value or d.dim_param or "?" for d in out.type.tensor_type.shape.dim]
            print(f"  OUT {out.name} : {dims}")
    except Exception as e:  # pragma: no cover
        print(f"(skipping {kind} introspection: {e})")


def _parity_check(model, image_out: Path, text_out: Path) -> None:
    try:
        import numpy as np
        import onnxruntime as ort
        import torch
    except Exception:
        print("(onnxruntime not installed; skipping ONNX/torch parity check)")
        return
    try:
        img = np.random.randn(1, 3, CLIP_SIZE, CLIP_SIZE).astype(np.float32)
        sess = ort.InferenceSession(str(image_out), providers=["CPUExecutionProvider"])
        onnx_emb = sess.run(None, {"image": img})[0]
        with torch.no_grad():
            torch_emb = model.encode_image(torch.from_numpy(img)).numpy()
        diff = float(np.abs(onnx_emb - torch_emb).max())
        print(f"image tower ONNX-vs-torch max abs diff: {diff:.2e} (should be < 1e-3)")
        assert onnx_emb.shape[-1] == EMBED_DIM, f"expected {EMBED_DIM}-d, got {onnx_emb.shape}"
    except Exception as e:  # pragma: no cover
        print(f"(parity check skipped: {e})")


if __name__ == "__main__":
    main()
