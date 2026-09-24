use base64::Engine;
use serde_json::Value;

const V1_PREFIX: &str = "ccp:codex:v1:";
const V2_PREFIX: &str = "ccp:codex:v2:";
const MAX_ID_BYTES: usize = 4 * 1024;
const MAX_ENCRYPTED_CONTENT_BYTES: usize = 8 * 1024 * 1024;

/// Whether `signature` was produced by this proxy's Codex reasoning replay
/// encoder.
///
/// These signatures embed the opaque `encrypted_content` replay blob, so
/// traffic captures redact the whole value. Keeping the check beside the
/// encoder keeps the format definition in one place.
pub(crate) fn is_proxy_reasoning_signature(signature: &str) -> bool {
    signature.starts_with(V1_PREFIX) || signature.starts_with(V2_PREFIX)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasoningReplay {
    pub id: String,
    pub summary: Vec<Value>,
    pub encrypted_content: String,
}

#[derive(Debug, Clone, Default)]
pub struct PendingReasoning {
    id: Option<String>,
    summary: Vec<Value>,
    encrypted_content: Option<String>,
}

impl PendingReasoning {
    pub fn capture(&mut self, item: &Value) {
        if let Some(id) = non_empty_string(item.get("id")) {
            self.id = Some(id.to_string());
        }
        if let Some(encrypted_content) = non_empty_string(item.get("encrypted_content")) {
            self.encrypted_content = Some(encrypted_content.to_string());
        }
        if let Some(summary) = item.get("summary").and_then(Value::as_array) {
            self.summary = summary.clone();
        }
    }

    pub fn replay(&self) -> Option<ReasoningReplay> {
        Some(ReasoningReplay {
            id: self.id.clone()?,
            summary: self.summary.clone(),
            encrypted_content: self.encrypted_content.clone()?,
        })
    }
}

pub fn encode_reasoning_signature(replay: &ReasoningReplay) -> Option<String> {
    if replay.id.is_empty()
        || replay.id.len() > MAX_ID_BYTES
        || replay.encrypted_content.is_empty()
        || replay.encrypted_content.len() > MAX_ENCRYPTED_CONTENT_BYTES
    {
        return None;
    }
    let payload = serde_json::to_vec(&serde_json::json!({
        "id": replay.id,
        "summary": replay.summary,
        "encrypted_content": replay.encrypted_content,
    }))
    .ok()?;
    if payload.len() > max_payload_len() {
        return None;
    }
    Some(format!(
        "{V2_PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
    ))
}

pub fn decode_reasoning_signature(signature: &str) -> Option<ReasoningReplay> {
    if let Some(payload) = signature.strip_prefix(V2_PREFIX) {
        if payload.is_empty() || payload.len() > encoded_payload_len_limit() {
            return None;
        }
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .ok()?;
        if decoded.len() > max_payload_len() {
            return None;
        }
        let value: Value = serde_json::from_slice(&decoded).ok()?;
        let id = non_empty_string(value.get("id"))?;
        let encrypted_content = non_empty_string(value.get("encrypted_content"))?;
        if id.len() > MAX_ID_BYTES || encrypted_content.len() > MAX_ENCRYPTED_CONTENT_BYTES {
            return None;
        }
        return Some(ReasoningReplay {
            id: id.to_string(),
            summary: value.get("summary")?.as_array()?.clone(),
            encrypted_content: encrypted_content.to_string(),
        });
    }
    let payload = signature.strip_prefix(V1_PREFIX)?;
    if payload.is_empty() || payload.len() > max_payload_len() {
        return None;
    }
    let (encoded_id, encrypted_content) = payload.split_once(':')?;
    if encoded_id.is_empty()
        || encoded_id.len() > encoded_id_len_limit()
        || encrypted_content.is_empty()
        || encrypted_content.len() > MAX_ENCRYPTED_CONTENT_BYTES
    {
        return None;
    }
    let id = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded_id)
        .ok()?;
    if id.is_empty() || id.len() > MAX_ID_BYTES {
        return None;
    }
    Some(ReasoningReplay {
        id: String::from_utf8(id).ok()?,
        summary: Vec::new(),
        encrypted_content: encrypted_content.to_string(),
    })
}

fn encoded_id_len_limit() -> usize {
    MAX_ID_BYTES.div_ceil(3) * 4
}

fn max_payload_len() -> usize {
    encoded_id_len_limit() + 1 + MAX_ENCRYPTED_CONTENT_BYTES
}

fn encoded_payload_len_limit() -> usize {
    max_payload_len().div_ceil(3) * 4
}

fn non_empty_string(value: Option<&Value>) -> Option<&str> {
    value?.as_str().filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn signature_round_trip_preserves_reasoning_identity() {
        let replay = ReasoningReplay {
            id: "rs_1".to_string(),
            summary: vec![json!({"type":"summary_text","text":"plan"})],
            encrypted_content: "gAAAAopaque".to_string(),
        };
        let signature = encode_reasoning_signature(&replay).unwrap();
        assert!(signature.starts_with(V2_PREFIX));
        assert_eq!(decode_reasoning_signature(&signature), Some(replay));
    }

    #[test]
    fn foreign_and_malformed_signatures_are_ignored() {
        assert_eq!(decode_reasoning_signature("anthropic-signature"), None);
        assert_eq!(decode_reasoning_signature("ccp:codex:v1:not-base64"), None);
    }

    #[test]
    fn pending_reasoning_keeps_early_metadata_when_done_omits_it() {
        let mut pending = PendingReasoning::default();
        pending.capture(&json!({
            "id": "rs_1",
            "summary": [{"type":"summary_text","text":"plan"}],
            "encrypted_content": "early"
        }));
        pending.capture(&json!({"id": "rs_1"}));
        assert_eq!(
            pending.replay(),
            Some(ReasoningReplay {
                id: "rs_1".to_string(),
                summary: vec![json!({"type":"summary_text","text":"plan"})],
                encrypted_content: "early".to_string(),
            })
        );
    }

    #[test]
    fn oversized_signature_is_ignored_without_decoding() {
        let signature = format!(
            "{V1_PREFIX}cnNfMQ:{}",
            "A".repeat(MAX_ENCRYPTED_CONTENT_BYTES + 1)
        );
        assert_eq!(decode_reasoning_signature(&signature), None);
    }
}
