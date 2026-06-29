//! Face detection + 5-point landmarks via SCRFD (InsightFace `scrfd_10g_bnkps`, ONNX). The default
//! detector: materially better small/distant-face recall than YuNet on WIDER-Hard, and it emits the
//! SAME 5-point landmark contract (`detect::Face`) so the rest of the pipeline is unchanged. SCRFD
//! landmarks are also the distribution ArcFace's `buffalo_l` pack was tuned against, so alignment
//! quality improves on top of recall.
//!
//! Preprocessing: letterbox to 640², RGB, `(x-127.5)/128` (InsightFace `1/128` scale, `127.5` mean,
//! pad with black pixels). Heads at strides 8/16/32 emit per-anchor score(1)/bbox(4: l,t,r,b in
//! stride units, distance-decoded from the anchor center)/kps(10). We read outputs by their LAST
//! DIM (1→score, 4→bbox, 10→kps) and order each group by anchor count (stride 8 has the most), so
//! the decode is robust to export tensor NAMES/ORDER. Validate against the real export with
//! `tests/vision_pipeline.rs::inspect_scrfd_model_io_shapes` at provisioning.

use anyhow::{Context, Result};
use image::RgbImage;
use ndarray::Array4;
use ort::session::Session;

use super::detect::{Face, FaceDetect};
use super::geom;

const INPUT: usize = 640;
const STRIDES: [usize; 3] = [8, 16, 32];

pub struct ScrfdDetector {
    session: Session,
    score_threshold: f32,
    nms_iou: f32,
}

impl ScrfdDetector {
    pub fn new(session: Session, score_threshold: f32) -> Self {
        Self {
            session,
            score_threshold,
            nms_iou: 0.4,
        }
    }
}

/// One ONNX output flattened with its trailing dimension (1=score, 4=bbox, 10=kps).
struct Head {
    last: usize,
    len: usize,
    data: Vec<f32>,
}

impl FaceDetect for ScrfdDetector {
    fn detect(&self, frame: &RgbImage) -> Result<Vec<Face>> {
        let (ow, oh) = (frame.width() as f32, frame.height() as f32);
        let scale = (INPUT as f32 / ow).min(INPUT as f32 / oh);
        let (nw, nh) = ((ow * scale).round() as u32, (oh * scale).round() as u32);
        let resized = image::imageops::resize(frame, nw, nh, image::imageops::FilterType::Triangle);

        // NCHW [1,3,640,640], RGB, (x-127.5)/128, black-padded (pad value = normalized pixel 0).
        let pad = (0.0 - 127.5) / 128.0;
        let mut input = Array4::<f32>::from_elem((1, 3, INPUT, INPUT), pad);
        for y in 0..nh as usize {
            for x in 0..nw as usize {
                let px = resized.get_pixel(x as u32, y as u32).0; // [R,G,B]
                for c in 0..3 {
                    input[[0, c, y, x]] = (px[c] as f32 - 127.5) / 128.0;
                }
            }
        }

        let outputs = self
            .session
            .run(ort::inputs![input].context("scrfd inputs")?)
            .context("scrfd inference")?;

        // Collect every output flattened, tagged by its trailing dim.
        let mut heads: Vec<Head> = Vec::new();
        for o in self.session.outputs.iter() {
            let t = outputs[o.name.as_str()]
                .try_extract_tensor::<f32>()
                .with_context(|| format!("extracting scrfd output {}", o.name))?;
            let shape = t.shape().to_vec();
            let last = *shape.last().unwrap_or(&1);
            let data: Vec<f32> = t.iter().copied().collect();
            heads.push(Head {
                last,
                len: data.len(),
                data,
            });
        }
        let group = |dim: usize| -> Result<Vec<Vec<f32>>> {
            let mut g: Vec<&Head> = heads.iter().filter(|h| h.last == dim).collect();
            anyhow::ensure!(
                g.len() == 3,
                "scrfd: expected 3 outputs with trailing dim {dim}, found {}",
                g.len()
            );
            // Stride 8 has the most anchors; order descending so index = [s8, s16, s32].
            g.sort_by(|a, b| b.len.cmp(&a.len));
            Ok(g.into_iter().map(|h| h.data.clone()).collect())
        };
        let scores = group(1)?;
        let bboxes = group(4)?;
        let kpss = group(10)?;

        let to_orig = |vx: f32, vy: f32| [vx / scale, vy / scale];
        let mut cands: Vec<Face> = Vec::new();
        for (si, &stride) in STRIDES.iter().enumerate() {
            let cols = INPUT / stride;
            let rows = INPUT / stride;
            let cells = rows * cols;
            let score = &scores[si];
            let bbox = &bboxes[si];
            let kps = &kpss[si];
            if cells == 0 || score.is_empty() {
                continue;
            }
            // Anchors-per-cell inferred from the score length (SCRFD-bnkps uses 2).
            let na = (score.len() / cells).max(1);
            for cell in 0..cells {
                let row = cell / cols;
                let col = cell % cols;
                let cx = (col * stride) as f32;
                let cy = (row * stride) as f32;
                for a in 0..na {
                    let idx = cell * na + a;
                    if idx >= score.len() {
                        break;
                    }
                    let s = score[idx];
                    if s < self.score_threshold {
                        continue;
                    }
                    let b = &bbox[idx * 4..idx * 4 + 4]; // l,t,r,b distance (×stride)
                    let x1 = cx - b[0] * stride as f32;
                    let y1 = cy - b[1] * stride as f32;
                    let x2 = cx + b[2] * stride as f32;
                    let y2 = cy + b[3] * stride as f32;
                    let tl = to_orig(x1, y1);
                    let br = to_orig(x2, y2);
                    let (w, h) = (br[0] - tl[0], br[1] - tl[1]);
                    if w <= 1.0 || h <= 1.0 {
                        continue;
                    }
                    let k = &kps[idx * 10..idx * 10 + 10];
                    let mut landmarks = [[0.0f32; 2]; 5];
                    for (j, lm) in landmarks.iter_mut().enumerate() {
                        let lx = cx + k[2 * j] * stride as f32;
                        let ly = cy + k[2 * j + 1] * stride as f32;
                        *lm = to_orig(lx, ly);
                    }
                    cands.push(Face {
                        bbox: [tl[0], tl[1], w, h],
                        score: s,
                        landmarks,
                    });
                }
            }
        }

        Ok(geom::nms_by(cands, self.nms_iou, |f| f.bbox, |f| f.score))
    }
}
