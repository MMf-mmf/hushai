//! Audition Kokoro speakers: writes one WAV per speaker id so you can pick the
//! voice for `RAG_TTS_SID`. Loads the model once and reuses it across ids.
//!
//!   RAG_TTS_DIR=models/kokoro-en-v0_19 \
//!     cargo run -p hushai-rag --example tts_audition -- "Some sentence." 5 6 9 10
//!
//! Then listen:  afplay /tmp/kokoro_sid6.wav
//!
//! kokoro-en-v0_19 speaker ids: 0 af, 1 af_bella, 2 af_nicole, 3 af_sarah,
//! 4 af_sky, 5 am_adam, 6 am_michael, 7 bf_emma, 8 bf_isabella, 9 bm_george,
//! 10 bm_lewis. Neutral American male = am_michael (6) or am_adam (5).

use sherpa_onnx::{
    GenerationConfig, OfflineTts, OfflineTtsConfig, OfflineTtsKokoroModelConfig,
    OfflineTtsModelConfig,
};

fn main() {
    let dir = std::env::var("RAG_TTS_DIR").unwrap_or_else(|_| "models/kokoro-en-v0_19".into());
    let mut args = std::env::args().skip(1);
    let text = args.next().unwrap_or_else(|| {
        "Hi, I'm your assistant. The package was delivered to the front porch around \
         four o'clock this afternoon."
            .into()
    });
    let sids: Vec<i32> = args.filter_map(|s| s.parse().ok()).collect();
    let sids = if sids.is_empty() {
        vec![5, 6, 9, 10]
    } else {
        sids
    };

    let config = OfflineTtsConfig {
        model: OfflineTtsModelConfig {
            kokoro: OfflineTtsKokoroModelConfig {
                model: Some(format!("{dir}/model.onnx")),
                voices: Some(format!("{dir}/voices.bin")),
                tokens: Some(format!("{dir}/tokens.txt")),
                data_dir: Some(format!("{dir}/espeak-ng-data")),
                ..Default::default()
            },
            num_threads: 2,
            ..Default::default()
        },
        ..Default::default()
    };

    let tts = OfflineTts::create(&config).expect("create Kokoro TTS engine");
    eprintln!(
        "loaded: sample_rate={} num_speakers={}",
        tts.sample_rate(),
        tts.num_speakers()
    );

    for sid in sids {
        let g = GenerationConfig {
            sid,
            speed: 1.0,
            ..Default::default()
        };
        let audio = tts
            .generate_with_config(&text, &g, None::<fn(&[f32], f32) -> bool>)
            .expect("generate");
        let out = format!("/tmp/kokoro_sid{sid}.wav");
        audio.save(&out);
        let secs = audio.samples().len() as f32 / audio.sample_rate() as f32;
        eprintln!("sid {sid}: wrote {out} ({secs:.1}s)");
    }
}
