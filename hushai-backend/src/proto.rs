//! The wire contract message (`hushai.v1.SegmentManifest`) and the single
//! proto→domain decode boundary.
//!
//! `pb` holds the prost-generated types compiled from
//! `proto/hushai/v1/segment.proto` by `build.rs`. [`DecodedManifest`] is the
//! validated domain view: it converts proto `bytes` ids into [`uuid::Uuid`] and
//! `uint64` fields into Postgres-friendly `i64` here, in ONE place — the
//! documented cast site (contract §4 → DB `bigint`).

use uuid::Uuid;

use crate::error::IngestError;

/// prost-generated types for package `hushai.v1`.
pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/hushai.v1.rs"));
}

pub use pb::{MediaType, SegmentManifest};

/// A validated, domain-typed manifest.
#[derive(Debug, Clone)]
pub struct DecodedManifest {
    pub segment_id: Uuid,
    pub device_id: String,
    pub stream_id: String,
    pub session_id: Uuid,
    pub sequence: i64,
    /// Stored verbatim; NEVER used to branch server behaviour (contract §7).
    pub source_kind: String,
    /// `hushai.v1.MediaType` numeric value; stored opaquely.
    pub media_type: i32,
    pub codec: String,
    pub container: String,
    pub codec_init_data: Vec<u8>,
    pub capture_start_unix_nanos: i64,
    pub monotonic_start_nanos: i64,
    pub duration_nanos: i64,
    pub content_sha256: [u8; 32],
    pub byte_len: i64,
    pub gap_before: bool,
    pub attrs: serde_json::Value,
}

impl DecodedManifest {
    /// Decode + validate a serialized `SegmentManifest`. All failures here are
    /// the client's fault (malformed manifest) → mapped to 400 by the caller.
    pub fn decode(bytes: &[u8]) -> Result<Self, IngestError> {
        use prost::Message;
        let m = SegmentManifest::decode(bytes)?;

        let segment_id = uuid_from_bytes("segment_id", &m.segment_id)?;
        let session_id = uuid_from_bytes("session_id", &m.session_id)?;

        if m.content_sha256.len() != 32 {
            return Err(IngestError::InvalidIdLength {
                field: "content_sha256",
                expected: 32,
                got: m.content_sha256.len(),
            });
        }
        let mut content_sha256 = [0u8; 32];
        content_sha256.copy_from_slice(&m.content_sha256);

        if m.device_id.is_empty() {
            return Err(IngestError::EmptyField("device_id"));
        }
        if m.stream_id.is_empty() {
            return Err(IngestError::EmptyField("stream_id"));
        }

        // proto map<string,string> -> jsonb object (never fails for string maps).
        let attrs = serde_json::to_value(&m.attrs).unwrap_or_else(|_| serde_json::json!({}));

        Ok(Self {
            segment_id,
            device_id: m.device_id,
            stream_id: m.stream_id,
            session_id,
            sequence: u64_to_i64("sequence", m.sequence)?,
            source_kind: m.source_kind,
            media_type: m.media_type,
            codec: m.codec,
            container: m.container,
            codec_init_data: m.codec_init_data,
            capture_start_unix_nanos: u64_to_i64("capture_start_unix_nanos", m.capture_start_unix_nanos)?,
            monotonic_start_nanos: u64_to_i64("monotonic_start_nanos", m.monotonic_start_nanos)?,
            duration_nanos: u64_to_i64("duration_nanos", m.duration_nanos)?,
            content_sha256,
            byte_len: u64_to_i64("byte_len", m.byte_len)?,
            gap_before: m.gap_before,
            attrs,
        })
    }
}

fn uuid_from_bytes(field: &'static str, raw: &[u8]) -> Result<Uuid, IngestError> {
    if raw.len() != 16 {
        return Err(IngestError::InvalidIdLength {
            field,
            expected: 16,
            got: raw.len(),
        });
    }
    let mut buf = [0u8; 16];
    buf.copy_from_slice(raw);
    Ok(Uuid::from_bytes(buf))
}

/// The documented `uint64` → `i64` cast at the proto/DB boundary. Values above
/// `i64::MAX` (≈9.2e18) are rejected rather than silently wrapped.
fn u64_to_i64(field: &'static str, v: u64) -> Result<i64, IngestError> {
    i64::try_from(v).map_err(|_| IngestError::ValueOutOfRange(field))
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    fn base() -> SegmentManifest {
        SegmentManifest {
            segment_id: vec![0u8; 16],
            session_id: vec![1u8; 16],
            content_sha256: vec![7u8; 32],
            device_id: "cam0".into(),
            stream_id: "cam0-muxed".into(),
            sequence: 5,
            media_type: MediaType::Muxed as i32,
            codec: "h264+aac".into(),
            container: "fmp4".into(),
            duration_nanos: 2_000_000_000,
            byte_len: 100,
            ..Default::default()
        }
    }

    #[test]
    fn decodes_valid_manifest() {
        let d = DecodedManifest::decode(&base().encode_to_vec()).unwrap();
        assert_eq!(d.sequence, 5);
        assert_eq!(d.media_type, 3);
        assert_eq!(d.content_sha256, [7u8; 32]);
        assert_eq!(d.segment_id.as_bytes(), &[0u8; 16]);
    }

    #[test]
    fn rejects_wrong_segment_id_length() {
        let mut m = base();
        m.segment_id = vec![0u8; 15];
        assert!(matches!(
            DecodedManifest::decode(&m.encode_to_vec()).unwrap_err(),
            IngestError::InvalidIdLength { field: "segment_id", .. }
        ));
    }

    #[test]
    fn rejects_wrong_sha_length() {
        let mut m = base();
        m.content_sha256 = vec![0u8; 31];
        assert!(matches!(
            DecodedManifest::decode(&m.encode_to_vec()).unwrap_err(),
            IngestError::InvalidIdLength { field: "content_sha256", .. }
        ));
    }

    #[test]
    fn rejects_empty_device_id() {
        let mut m = base();
        m.device_id = String::new();
        assert!(matches!(
            DecodedManifest::decode(&m.encode_to_vec()).unwrap_err(),
            IngestError::EmptyField("device_id")
        ));
    }

    #[test]
    fn rejects_u64_field_above_i64_max() {
        let mut m = base();
        m.duration_nanos = u64::MAX;
        assert!(matches!(
            DecodedManifest::decode(&m.encode_to_vec()).unwrap_err(),
            IngestError::ValueOutOfRange("duration_nanos")
        ));
    }
}
