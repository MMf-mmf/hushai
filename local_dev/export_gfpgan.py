#!/usr/bin/env python3
"""Export a blind-face-restoration model (GFPGANv1.4 default, or CodeFormer) to ONNX for the worker's
image-cleanup stage (hushai-worker/src/vision/enhance.rs :: FaceRestorer).

WHY (see AGENTS.md "vision"): the cleanup cascade RESTORES low-quality face crops (small / blurry /
distant) before ArcFace so they can be recognized instead of dropped ("recover-then-embed"). The
restorer takes an aligned-ish 512x512 RGB face normalized to [-1,1] (NCHW) and returns a restored
512x512 RGB face in [-1,1]. enhance.rs reads the output by ORDER; validate the real export with:
    cargo test -p hushai-worker --test vision_pipeline inspect_enhance_model_io_shapes -- --nocapture
    FACE_TEST_DIR=/path cargo test -p hushai-worker --test vision_pipeline \
        restored_low_quality_recovers_identity -- --nocapture   # the recover-then-embed gate

GFPGANv1.4 is more identity-faithful (lower embedding drift) — the default. CodeFormer is stronger on
severe degradation and takes an extra scalar fidelity weight `w` (FaceRestorer passes FACE_RESTORE_
CODEFORMER_W when the graph has 2 inputs). Pick per FACE_RESTORE_KIND.

LICENSE: GFPGAN / CodeFormer weights are research/non-commercial; weights stay gitignored under
models/, the code path is MIT (same posture as ArcFace). Requires (the script prints the pip line if
a dep is missing): torch, and the chosen model's package (gfpgan / basicsr, or the codeformer repo).

Output (gitignored under models/):
    models/gfpgan_v1.4.onnx        default FACE_RESTORE_MODEL_PATH (FACE_RESTORE_KIND=gfpgan)
    models/codeformer.onnx         when run with --model codeformer (FACE_RESTORE_KIND=codeformer)

Usage:
    python3 local_dev/export_gfpgan.py                  # GFPGANv1.4 -> models/gfpgan_v1.4.onnx
    python3 local_dev/export_gfpgan.py --model codeformer
    python3 local_dev/export_gfpgan.py --force
"""
from __future__ import annotations

import argparse
import hashlib
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
MODELS_DIR = REPO_ROOT / "models"
SIZE = 512


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
    ap.add_argument("--model", choices=["gfpgan", "codeformer"], default="gfpgan")
    ap.add_argument("--force", action="store_true")
    ap.add_argument("--opset", type=int, default=17)
    args = ap.parse_args()

    try:
        import torch  # noqa: F401
    except ImportError:
        die("Missing torch. Install: pip install torch")

    out = MODELS_DIR / ("gfpgan_v1.4.onnx" if args.model == "gfpgan" else "codeformer.onnx")
    if out.exists() and not args.force:
        print(f"{out} already exists (use --force to re-export). sha256: {sha256(out)}")
        return 0
    MODELS_DIR.mkdir(parents=True, exist_ok=True)

    import torch

    if args.model == "gfpgan":
        try:
            from gfpgan.archs.gfpganv1_clean_arch import GFPGANv1Clean
        except ImportError:
            die("Missing gfpgan. Install: pip install gfpgan basicsr facexlib")
        from urllib.request import urlretrieve

        ckpt = MODELS_DIR / "GFPGANv1.4.pth"
        if not ckpt.exists():
            url = "https://github.com/TencentARC/GFPGAN/releases/download/v1.3.4/GFPGANv1.4.pth"
            print(f"Fetching {url} …")
            urlretrieve(url, ckpt)
        model = GFPGANv1Clean(
            out_size=SIZE, num_style_feat=512, channel_multiplier=2,
            decoder_load_path=None, fix_decoder=False, num_mlp=8,
            input_is_latent=True, different_w=True, narrow=1, sft_half=True,
        )
        state = torch.load(ckpt, map_location="cpu")
        model.load_state_dict(state["params_ema"] if "params_ema" in state else state, strict=False)
        model.eval()

        class Wrap(torch.nn.Module):
            def __init__(self, m):
                super().__init__()
                self.m = m

            def forward(self, x):
                # GFPGANv1Clean returns (image, rgb_list); we want the restored image only.
                return self.m(x, return_rgb=False)[0]

        net, dummy, inputs, names = Wrap(model), torch.randn(1, 3, SIZE, SIZE), ["input"], ["output"]
        dyn = None
    else:
        die(
            "CodeFormer export: clone https://github.com/sczhou/CodeFormer, load the released\n"
            "weights into its arch, wrap forward to return only the restored image (and expose the\n"
            "scalar fidelity input `w`), then torch.onnx.export at 512x512. See that repo's README;\n"
            "this stub documents the contract enhance.rs expects (2 inputs: image[-1,1], w[double])."
        )

    print(f"Exporting {args.model} to {out} (opset {args.opset}) …")
    torch.onnx.export(
        net, dummy, str(out), input_names=inputs, output_names=names,
        opset_version=args.opset, dynamic_axes=dyn,
    )
    print(f"Done. sha256: {sha256(out)}")
    print("Validate: cargo test -p hushai-worker --test vision_pipeline inspect_enhance_model_io_shapes -- --nocapture")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
