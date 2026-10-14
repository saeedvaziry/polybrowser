//! Decrypting the cookie values that Chromium-family browsers store on macOS.
//!
//! Chromium encrypts each cookie value with AES-128-CBC. The key is derived
//! with PBKDF2-HMAC-SHA1 from a password kept in the login keychain under the
//! service `"<Browser> Safe Storage"`. Newer Chromium also prepends a SHA-256
//! of the cookie's host to the plaintext as a bound-domain check.

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use sha2::{Digest, Sha256};

/// Salt, iteration count and key length are fixed by Chromium.
const SALT: &[u8] = b"saltysalt";
const ITERATIONS: u32 = 1003;
/// AES-128 uses the first 16 bytes; Chromium pads the IV with spaces.
const IV: [u8; 16] = [b' '; 16];

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

/// Derives the AES key from the keychain password.
pub(super) fn derive_key(password: &[u8]) -> [u8; 16] {
    pbkdf2::pbkdf2_hmac_array::<sha1::Sha1, 16>(password, SALT, ITERATIONS)
}

/// Turns a stored cookie value into its plaintext.
///
/// `encrypted_value` is the `encrypted_value` column, `plain_value` the
/// `value` column (used when the first is empty), and `host_key` the cookie's
/// host, needed to strip the domain-bound prefix newer Chromium adds.
pub(super) fn decrypt_value(
    key: &[u8; 16],
    encrypted_value: &[u8],
    plain_value: &str,
    host_key: &str,
) -> Option<String> {
    // Empty encrypted column: the value was never encrypted.
    if encrypted_value.is_empty() {
        return Some(plain_value.to_owned());
    }
    // `v10`/`v11` mark an encrypted value; anything else is stored raw.
    let ciphertext = match encrypted_value {
        [b'v', b'1', b'0', rest @ ..] | [b'v', b'1', b'1', rest @ ..] => rest,
        raw => return Some(String::from_utf8_lossy(raw).into_owned()),
    };

    let mut buffer = ciphertext.to_vec();
    let plaintext = Aes128CbcDec::new(key.into(), &IV.into())
        .decrypt_padded_mut::<Pkcs7>(&mut buffer)
        .ok()?;

    Some(String::from_utf8_lossy(strip_domain_hash(plaintext, host_key)).into_owned())
}

/// Newer Chromium prepends `SHA256(host_key)` (32 bytes) to the plaintext.
/// Remove it when it is there.
fn strip_domain_hash<'a>(plaintext: &'a [u8], host_key: &str) -> &'a [u8] {
    if plaintext.len() > 32 {
        let hash = Sha256::digest(host_key.as_bytes());
        if plaintext[..32] == hash[..] {
            return &plaintext[32..];
        }
    }
    plaintext
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};

    type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;

    /// Encrypts the way Chromium does, for round-trip tests.
    fn encrypt(key: &[u8; 16], prefix: &[u8], plaintext: &[u8]) -> Vec<u8> {
        let ciphertext =
            Aes128CbcEnc::new(key.into(), &IV.into()).encrypt_padded_vec_mut::<Pkcs7>(plaintext);
        [prefix, &ciphertext].concat()
    }

    #[test]
    fn derives_a_stable_key() {
        // A fixed password gives a fixed 16-byte key.
        let key = derive_key(b"peanuts");
        assert_eq!(key.len(), 16);
        assert_eq!(derive_key(b"peanuts"), key);
        assert_ne!(derive_key(b"other"), key);
    }

    #[test]
    fn decrypts_v10_values() {
        let key = derive_key(b"secret");
        let encrypted = encrypt(&key, b"v10", b"session=abc");
        assert_eq!(
            decrypt_value(&key, &encrypted, "", "example.com").as_deref(),
            Some("session=abc")
        );
    }

    #[test]
    fn strips_the_domain_hash_prefix() {
        let key = derive_key(b"secret");
        let host = "example.com";
        let mut plaintext = Sha256::digest(host.as_bytes()).to_vec();
        plaintext.extend_from_slice(b"token=42");
        let encrypted = encrypt(&key, b"v10", &plaintext);
        assert_eq!(
            decrypt_value(&key, &encrypted, "", host).as_deref(),
            Some("token=42")
        );
    }

    #[test]
    fn keeps_a_32_plus_byte_value_without_the_hash() {
        let key = derive_key(b"secret");
        // Looks long enough for a prefix, but the first 32 bytes are not the
        // host hash, so nothing is stripped.
        let plaintext = b"0123456789012345678901234567890123".as_slice();
        let encrypted = encrypt(&key, b"v11", plaintext);
        assert_eq!(
            decrypt_value(&key, &encrypted, "", "example.com").as_deref(),
            Some(std::str::from_utf8(plaintext).unwrap())
        );
    }

    #[test]
    fn falls_back_to_the_plain_value_column() {
        let key = derive_key(b"secret");
        assert_eq!(
            decrypt_value(&key, b"", "legacy=1", "example.com").as_deref(),
            Some("legacy=1")
        );
    }

    #[test]
    fn treats_unprefixed_values_as_raw() {
        let key = derive_key(b"secret");
        assert_eq!(
            decrypt_value(&key, b"rawbytes", "", "example.com").as_deref(),
            Some("rawbytes")
        );
    }
}
