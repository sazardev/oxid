//! AES-256-GCM secret encryption (SPEC.md §4.4).
//!
//! A 256-bit master key encrypts every secret before it touches the database.
//! Ciphertexts are stored as base64(`nonce ‖ ciphertext‖ tag`) so each value
//! uses an independent random nonce and needs no side-channel bookkeeping.

use std::path::Path;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use rand::RngCore;
use rand::rngs::OsRng;

/// Errors surfaced while encrypting or decrypting secrets.
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    /// The key file could not be read or written.
    #[error("key file failure: {0}")]
    KeyFile(String),
    /// Encryption or decryption of a value failed.
    #[error("crypto failure: {0}")]
    Operation(String),
}

/// A 256-bit master key wrapped around AES-256-GCM.
#[derive(Debug, Clone)]
pub struct Cipher {
    key: [u8; 32],
}

impl Cipher {
    /// Builds a cipher from a caller-supplied 32-byte key.
    #[must_use]
    pub fn from_key(key: [u8; 32]) -> Self {
        Self { key }
    }

    /// Generates a fresh random key (first-run initialisation).
    #[must_use]
    pub fn generate() -> Self {
        let mut key = [0u8; 32];
        OsRng.fill_bytes(&mut key);
        Self { key }
    }

    /// Loads the key from `path`, generating and persisting one if absent.
    ///
    /// # Errors
    /// Returns [`CryptoError::KeyFile`] if the key cannot be persisted.
    pub fn load_or_create(path: &Path) -> Result<Self, CryptoError> {
        if path.exists() {
            let raw = std::fs::read(path).map_err(|e| CryptoError::KeyFile(e.to_string()))?;
            let key: [u8; 32] = raw.try_into().map_err(|_| {
                CryptoError::KeyFile(format!("`{}` is not a 32-byte key", path.display()))
            })?;
            return Ok(Self { key });
        }
        let cipher = Self::generate();
        std::fs::create_dir_all(path.parent().unwrap_or_else(|| std::path::Path::new(".")))
            .map_err(|e| CryptoError::KeyFile(e.to_string()))?;
        std::fs::write(path, cipher.key).map_err(|e| CryptoError::KeyFile(e.to_string()))?;
        Ok(cipher)
    }

    /// Encrypts `plaintext`, returning base64(`nonce ‖ ciphertext‖ tag`).
    ///
    /// # Errors
    /// Returns [`CryptoError::Operation`] if the key is misconfigured.
    pub fn encrypt(&self, plaintext: &str) -> Result<String, CryptoError> {
        let cipher = Aes256Gcm::new_from_slice(&self.key)
            .map_err(|e| CryptoError::Operation(e.to_string()))?;
        let mut nonce = [0u8; 12];
        OsRng.fill_bytes(&mut nonce);
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce), plaintext.as_bytes())
            .map_err(|e| CryptoError::Operation(e.to_string()))?;

        let mut blob = Vec::with_capacity(nonce.len() + ciphertext.len());
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(&ciphertext);
        Ok(hex::encode(blob))
    }

    /// Decrypts a value produced by [`Cipher::encrypt`].
    ///
    /// # Errors
    /// Returns [`CryptoError::Operation`] if the blob is malformed or the key
    /// does not match.
    pub fn decrypt(&self, encoded: &str) -> Result<String, CryptoError> {
        let blob = hex::decode(encoded)
            .map_err(|e| CryptoError::Operation(format!("invalid ciphertext: {e}")))?;
        if blob.len() < 12 {
            return Err(CryptoError::Operation("ciphertext too short".to_owned()));
        }
        let (nonce, ciphertext) = blob.split_at(12);
        let cipher = Aes256Gcm::new_from_slice(&self.key)
            .map_err(|e| CryptoError::Operation(e.to_string()))?;
        let plaintext = cipher
            .decrypt(Nonce::from_slice(nonce), ciphertext)
            .map_err(|_| {
                CryptoError::Operation("decryption failed (wrong master key?)".to_owned())
            })?;
        String::from_utf8(plaintext)
            .map_err(|e| CryptoError::Operation(format!("decrypted value is not UTF-8: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cipher() -> Cipher {
        Cipher::from_key([7u8; 32])
    }

    #[test]
    fn roundtrip_preserves_value() {
        let c = cipher();
        let secret = "postgres://user:pa$$w0rd@localhost:5432/db";
        let encoded = c.encrypt(secret).unwrap();
        assert_ne!(encoded, hex::encode(secret));
        assert_eq!(c.decrypt(&encoded).unwrap(), secret);
    }

    #[test]
    fn each_encryption_uses_fresh_nonce() {
        let c = cipher();
        let a = c.encrypt("same-value").unwrap();
        let b = c.encrypt("same-value").unwrap();
        assert_ne!(a, b);
        assert_eq!(c.decrypt(&a).unwrap(), c.decrypt(&b).unwrap());
    }

    #[test]
    fn wrong_key_cannot_decrypt() {
        let other = Cipher::from_key([9u8; 32]);
        let encoded = cipher().encrypt("top-secret").unwrap();
        assert!(other.decrypt(&encoded).is_err());
    }

    #[test]
    fn generates_and_reloads_key_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.key");
        let first = Cipher::load_or_create(&path).unwrap();
        let second = Cipher::load_or_create(&path).unwrap();
        let encoded = first.encrypt("value").unwrap();
        assert_eq!(second.decrypt(&encoded).unwrap(), "value");
    }
}
