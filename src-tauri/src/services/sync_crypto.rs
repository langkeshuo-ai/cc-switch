//! Opt-in end-to-end encryption for sync snapshots (transport-agnostic).
//!
//! Implemented exactly once here and reused by the shared snapshot flows in
//! [`super::sync_protocol`], so WebDAV and S3 can never drift on wire format or
//! cryptographic parameters.
//!
//! Wire format (self-describing, so legacy plaintext artifacts keep working):
//!
//! ```text
//! offset  0 : magic  "CCSYNC01"        (8 bytes)
//! offset  8 : Argon2id salt            (16 bytes, random per file)
//! offset 24 : AES-256-GCM nonce        (12 bytes, random per file)
//! offset 36 : ciphertext || GCM tag    (variable)
//! ```
//!
//! The key is derived from the user password with Argon2id. A fresh salt and
//! nonce per artifact mean identical plaintext never yields identical remote
//! bytes.
//!
//! Disk exposure on upload: the database export is serialized and encrypted
//! **entirely in memory**. The skills archive is not — the SSOT zipper only
//! writes to a path, so plaintext `skills.zip` briefly lands in the system
//! temp directory before it is read back into memory and encrypted (the temp
//! directory is removed when the snapshot build returns). Only the encrypted
//! bytes reach the remote. On download, artifacts are decrypted in memory and
//! never written to disk in plaintext.
//!
//! # Threat model
//!
//! - **Artifacts**: confidentiality + integrity via AES-256-GCM (the AEAD tag
//!   covers the ciphertext, so tampering is detected on decrypt).
//! - **Manifest**: authenticity via HMAC-SHA256 ([`sign_manifest`] /
//!   [`verify_manifest_mac`]), keyed from the same E2E password with a
//!   *separate* derivation domain. This closes the "attacker who controls the
//!   remote recomputes a self-consistent manifest + artifacts" hole: without a
//!   key derived from a secret the attacker does not have, they cannot produce a
//!   manifest that verifies.
//!
//! Remaining gap: **replay**. A valid, correctly signed *older* manifest still
//! verifies. Preventing that needs a monotonic counter or timestamp freshness
//! window, which requires server-side state this client does not have. Signing
//! at least makes downgrade require the attacker to hold a captured signed
//! manifest, and it makes silent tampering impossible.
//!
//! The manifest MAC is only possible when end-to-end encryption is enabled (it
//! is keyed from that password). Snapshots synced without E2E keep the previous
//! transport-only trust model — no signature is written, and verification skips
//! rather than silently accepting an unsigned manifest as if it were signed.

use aes_gcm::aead::rand_core::RngCore;
use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::error::AppError;

use super::sync_protocol::localized;

/// Magic prefix marking an encrypted artifact. Chosen to never collide with the
/// SQL or ZIP plaintext payloads (`db.sql`, `skills.zip`).
pub(crate) const ENCRYPTED_MAGIC: &[u8; 8] = b"CCSYNC01";

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;
const HEADER_LEN: usize = ENCRYPTED_MAGIC.len() + SALT_LEN + NONCE_LEN;

// Argon2id parameters: 19 MiB memory, 2 passes, single lane. This is the
// OWASP-recommended baseline and mirrors the library default.
const ARGON2_MEM_KIB: u32 = 19 * 1024;
const ARGON2_ITERS: u32 = 2;
const ARGON2_LANES: u32 = 1;

/// True when `bytes` carries the encrypted-artifact magic header.
pub(crate) fn is_encrypted_blob(bytes: &[u8]) -> bool {
    bytes.len() >= ENCRYPTED_MAGIC.len() && &bytes[..ENCRYPTED_MAGIC.len()] == ENCRYPTED_MAGIC
}

fn derive_key(password: &str, salt: &[u8]) -> Result<[u8; KEY_LEN], AppError> {
    let params =
        Params::new(ARGON2_MEM_KIB, ARGON2_ITERS, ARGON2_LANES, Some(KEY_LEN)).map_err(|e| {
            localized(
                "sync.encryption.kdf_params_invalid",
                format!("端到端加密参数无效: {e}"),
                format!("Invalid end-to-end encryption parameters: {e}"),
            )
        })?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = [0u8; KEY_LEN];
    argon2
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|e| {
            localized(
                "sync.encryption.kdf_failed",
                format!("口令派生密钥失败: {e}"),
                format!("Failed to derive a key from the password: {e}"),
            )
        })?;
    Ok(key)
}

/// Domain separator for the manifest MAC key derivation.
///
/// The manifest MAC key must be **independent** of the per-artifact encryption
/// keys, otherwise it would need the artifact's random salt to re-derive — and
/// the manifest is verified *before* any artifact is decrypted, so that salt is
/// not available yet. A fixed, distinct salt gives both properties at once: the
/// same password always yields the same MAC key on every device (so one device
/// can verify a snapshot another device uploaded), and the MAC key shares no
/// bytes with any encryption key.
const MANIFEST_MAC_SALT: &[u8] = b"cc-switch:manifest-mac:v1";

/// Compute the HMAC-SHA256 tag (hex) authenticating a manifest's canonical bytes.
///
/// `payload` must be the exact bytes that were signed — for a manifest, the
/// canonical serialization with the `mac` field excluded (see
/// [`super::sync_protocol::manifest_signing_bytes`]).
pub(crate) fn manifest_mac(payload: &[u8], password: &str) -> Result<String, AppError> {
    let key = derive_key(password, MANIFEST_MAC_SALT)?;
    // `Hmac` 同时实现 `Mac` 与 `KeyInit`，两者都有 `new_from_slice`，必须完全
    // 限定。`Mac::new_from_slice` 对任意长度密钥都成功（HMAC 会自行规范化到
    // block size），故这里的长度检查是 `Mac` trait 的固定要求，不是真实失败点。
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&key).map_err(|e| {
        localized(
            "sync.manifest.mac_init_failed",
            format!("清单签名器初始化失败: {e}"),
            format!("Failed to initialize the manifest authenticator: {e}"),
        )
    })?;
    mac.update(payload);
    Ok(mac
        .finalize()
        .into_bytes()
        .iter()
        .fold(String::with_capacity(64), |mut acc, b| {
            use std::fmt::Write as _;
            let _ = write!(acc, "{b:02x}");
            acc
        }))
}

/// Encrypt `plaintext` into a self-describing blob (magic + salt + nonce + AEAD
/// ciphertext). A fresh salt/nonce is generated on every call.
pub(crate) fn encrypt_blob(plaintext: &[u8], password: &str) -> Result<Vec<u8>, AppError> {
    let mut salt = [0u8; SALT_LEN];
    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut salt);
    OsRng.fill_bytes(&mut nonce);

    let key = derive_key(password, &salt)?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), plaintext)
        .map_err(|_| {
            localized(
                "sync.encryption.encrypt_failed",
                "加密快照失败",
                "Failed to encrypt the snapshot",
            )
        })?;

    let mut out = Vec::with_capacity(HEADER_LEN + ciphertext.len());
    out.extend_from_slice(ENCRYPTED_MAGIC);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Decrypt a blob produced by [`encrypt_blob`].
///
/// A wrong password or a corrupted file fails the AEAD authentication tag and
/// returns a single, explicit error. Corrupted data is never returned.
pub(crate) fn decrypt_blob(data: &[u8], password: &str) -> Result<Vec<u8>, AppError> {
    if !is_encrypted_blob(data) {
        return Err(localized(
            "sync.encryption.not_encrypted",
            "数据不是加密快照",
            "Data is not an encrypted snapshot",
        ));
    }
    if data.len() < HEADER_LEN {
        return Err(localized(
            "sync.encryption.truncated",
            "加密快照已损坏（头部不完整）",
            "Encrypted snapshot is corrupted (truncated header)",
        ));
    }

    let salt = &data[ENCRYPTED_MAGIC.len()..ENCRYPTED_MAGIC.len() + SALT_LEN];
    let nonce = &data[ENCRYPTED_MAGIC.len() + SALT_LEN..HEADER_LEN];
    let ciphertext = &data[HEADER_LEN..];

    let key = derive_key(password, salt)?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
    cipher
        .decrypt(Nonce::from_slice(nonce), ciphertext)
        .map_err(|_| {
            localized(
                "sync.encryption.decrypt_failed",
                "解密快照失败：口令错误或文件已损坏",
                "Failed to decrypt the snapshot: wrong password or corrupted file",
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_then_decrypt_round_trips() {
        let plaintext = b"cc-switch snapshot payload \x00\x01\x02";
        let blob = encrypt_blob(plaintext, "correct horse battery staple").expect("encrypt");
        assert!(is_encrypted_blob(&blob));
        assert_ne!(
            &blob[..],
            plaintext,
            "ciphertext must differ from plaintext"
        );
        let restored = decrypt_blob(&blob, "correct horse battery staple").expect("decrypt");
        assert_eq!(restored, plaintext);
    }

    #[test]
    fn encrypt_is_nondeterministic() {
        let a = encrypt_blob(b"same", "pw").expect("encrypt a");
        let b = encrypt_blob(b"same", "pw").expect("encrypt b");
        assert_ne!(a, b, "random salt/nonce must make each blob unique");
    }

    #[test]
    fn decrypt_with_wrong_password_is_rejected() {
        let blob = encrypt_blob(b"secret", "right").expect("encrypt");
        let err = decrypt_blob(&blob, "wrong").expect_err("wrong password must fail");
        assert!(
            err.to_string().contains("口令错误")
                || err.to_string().contains("wrong password")
                || err.to_string().contains("corrupted"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn decrypt_detects_corruption() {
        let mut blob = encrypt_blob(b"secret payload", "pw").expect("encrypt");
        let last = blob.len() - 1;
        blob[last] ^= 0xff;
        assert!(decrypt_blob(&blob, "pw").is_err(), "tampered tag must fail");
    }

    #[test]
    fn plaintext_is_not_detected_as_encrypted() {
        assert!(!is_encrypted_blob(b"CREATE TABLE providers(id);"));
        assert!(!is_encrypted_blob(b""));
        assert!(!is_encrypted_blob(b"PK\x03\x04"));
    }

    #[test]
    fn decrypt_rejects_plaintext_input() {
        assert!(decrypt_blob(b"not encrypted", "pw").is_err());
    }
}
