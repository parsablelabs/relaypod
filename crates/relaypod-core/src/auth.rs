use rand::RngCore;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

/// Generates a random bearer token prefixed with `rp_`.
pub fn generate_token() -> String {
    let mut bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    format!("rp_{}", hex::encode(bytes))
}

/// Computes the hex-encoded SHA-256 hash of a bearer token.
pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

/// Extracts unique lowercase `@handle` mentions from a message body.
/// Handles may contain ASCII alphanumerics, `-`, and `_`.
pub fn extract_mentions(body: &str) -> Vec<String> {
    let mut handles = BTreeSet::new();
    let bytes = body.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'@' {
            // Avoid matching email addresses like user@domain.com
            let prev_is_word = i > 0
                && (bytes[i - 1].is_ascii_alphanumeric()
                    || bytes[i - 1] == b'_'
                    || bytes[i - 1] == b'-');
            if !prev_is_word {
                let start = i + 1;
                let mut end = start;
                while end < bytes.len()
                    && (bytes[end].is_ascii_alphanumeric()
                        || bytes[end] == b'-'
                        || bytes[end] == b'_')
                {
                    end += 1;
                }
                if end > start {
                    handles.insert(body[start..end].to_lowercase());
                }
                i = end;
                continue;
            }
        }
        i += 1;
    }

    handles.into_iter().collect()
}

pub fn normalize_handle(raw: &str) -> String {
    raw.trim().trim_start_matches('@').to_lowercase()
}

pub fn normalize_channel_name(raw: &str) -> String {
    raw.trim().trim_start_matches('#').to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_mentions() {
        let m = extract_mentions(
            "Hey @claude-local and @bob_agy, email foo@bar.com, ping @Claude-Local again!",
        );
        assert_eq!(m, vec!["bob_agy".to_string(), "claude-local".to_string()]);
    }

    #[test]
    fn test_token_hash_roundtrip() {
        let t = generate_token();
        assert!(t.starts_with("rp_"));
        assert_eq!(hash_token(&t).len(), 64);
        assert_eq!(hash_token(&t), hash_token(&t));
    }
}
