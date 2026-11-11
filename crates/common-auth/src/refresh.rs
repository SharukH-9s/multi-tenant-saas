use std::fmt;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use sha2::{Digest, Sha256};

/// A freshly generated refresh token.
///
/// `plain` goes to the client once and is never stored. `hash` is what the
/// database keeps, so a leaked table cannot be used to refresh sessions.
pub struct RefreshToken {
    pub plain: String,
    pub hash: [u8; 32],
}

// Manual Debug so the plain token can never end up in logs.
impl fmt::Debug for RefreshToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RefreshToken")
            .field("plain", &"<redacted>")
            .field("hash", &"<redacted>")
            .finish()
    }
}

/// 32 random bytes from the OS-seeded CSPRNG, encoded as URL-safe base64.
pub fn generate_refresh_token() -> RefreshToken {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes[..]);

    let plain = URL_SAFE_NO_PAD.encode(bytes);
    let hash = hash_refresh_token(&plain);
    RefreshToken { plain, hash }
}

/// SHA-256 of the token as the client sends it. Used to look the token up.
/// A plain fast hash is fine here because the input is 256 bits of randomness.
pub fn hash_refresh_token(plain: &str) -> [u8; 32] {
    Sha256::digest(plain.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_43_url_safe_characters() {
        let token = generate_refresh_token();
        assert_eq!(token.plain.len(), 43); // 32 bytes, base64url, no padding
        assert!(token
            .plain
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn tokens_are_unique() {
        assert_ne!(
            generate_refresh_token().plain,
            generate_refresh_token().plain
        );
    }

    #[test]
    fn stored_hash_matches_rehashing_the_plain_token() {
        let token = generate_refresh_token();
        assert_eq!(token.hash, hash_refresh_token(&token.plain));
        assert_ne!(token.hash, hash_refresh_token("some other token"));
    }

    #[test]
    fn debug_output_hides_the_token() {
        let token = generate_refresh_token();
        let text = format!("{token:?}");
        assert!(!text.contains(&token.plain));
    }
}
