use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use rand::RngCore;

/// AES-256-GCM nonce length in bytes.
const NONCE_LEN: usize = 12;

/// Symmetric cipher for at-rest protection of per-user Commons bot-password tokens.
///
/// The 32-byte key comes from the `CREDENTIAL_ENC_KEY` environment variable (base64),
/// set once in `terraform.tfvars` like the Telegram token. A KMS key would cost
/// $1/month; this keeps the hobby project on the free tier while keeping stored
/// tokens opaque to anyone who can only read the DynamoDB table.
#[derive(Clone)]
pub struct Cipher {
    key: [u8; 32],
}

impl Cipher {
    /// Builds a cipher from the base64-encoded 32-byte master key.
    pub fn from_base64_key(value: &str) -> Result<Self> {
        let raw = BASE64
            .decode(value.trim())
            .context("CREDENTIAL_ENC_KEY must be valid base64")?;
        if raw.len() != 32 {
            bail!(
                "CREDENTIAL_ENC_KEY must decode to 32 bytes, got {}",
                raw.len()
            );
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&raw);
        Ok(Self { key })
    }

    /// Encrypts a token, returning base64(nonce ‖ ciphertext ‖ tag).
    pub fn encrypt(&self, plaintext: &str) -> Result<String> {
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&self.key));
        let mut nonce_bytes = [0u8; NONCE_LEN];
        rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = cipher
            .encrypt(nonce, plaintext.as_bytes())
            .map_err(|error| anyhow!("AES-GCM encryption failed: {error}"))?;
        let mut combined = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        combined.extend_from_slice(&nonce_bytes);
        combined.extend_from_slice(&ciphertext);
        Ok(BASE64.encode(combined))
    }

    /// Decrypts a base64(nonce ‖ ciphertext ‖ tag) blob back into the token.
    pub fn decrypt(&self, encoded: &str) -> Result<String> {
        let combined = BASE64
            .decode(encoded.trim())
            .context("stored credential is not valid base64")?;
        if combined.len() <= NONCE_LEN {
            bail!("stored credential ciphertext is too short");
        }
        let (nonce_bytes, ciphertext) = combined.split_at(NONCE_LEN);
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&self.key));
        let plaintext = cipher
            .decrypt(Nonce::from_slice(nonce_bytes), ciphertext)
            .map_err(|error| anyhow!("AES-GCM decryption failed: {error}"))?;
        String::from_utf8(plaintext).context("decrypted credential is not valid UTF-8")
    }
}

#[cfg(test)]
mod tests {
    use super::Cipher;
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as BASE64;

    fn test_cipher() -> Cipher {
        Cipher::from_base64_key(&BASE64.encode([7u8; 32])).unwrap()
    }

    #[test]
    fn round_trips_a_token() {
        let cipher = test_cipher();
        let token = "User@bot 0123456789abcdef0123456789abcdef";
        let encrypted = cipher.encrypt(token).unwrap();
        assert_ne!(encrypted, token);
        assert_eq!(cipher.decrypt(&encrypted).unwrap(), token);
    }

    #[test]
    fn distinct_nonces_make_ciphertexts_differ() {
        let cipher = test_cipher();
        assert_ne!(
            cipher.encrypt("same").unwrap(),
            cipher.encrypt("same").unwrap()
        );
    }

    #[test]
    fn rejects_wrong_key_length() {
        assert!(Cipher::from_base64_key(&BASE64.encode([0u8; 16])).is_err());
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let cipher = test_cipher();
        let mut encrypted = cipher.encrypt("secret").unwrap();
        encrypted.push('A');
        assert!(cipher.decrypt(&encrypted).is_err());
    }
}
