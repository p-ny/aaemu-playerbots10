//! Session keys for the C->S cipher and their RSA wrapping.
//!
//! `X2EnterWorldResponse` carries the server's RSA-1024 public key as a 260-byte blob:
//! `dwKeySize(i32=1024) | modulus(128, big-endian) | 125 zero bytes | exponent(3, big-endian)`.
//! We generate a random AES key and XOR blob, PKCS#1 v1.5-encrypt each, and send them in `CSAesXorKey`.

use rand::{CryptoRng, RngCore};
use rsa::{BigUint, Pkcs1v15Encrypt, RsaPublicKey};

use super::ctos::CsCipher;
use crate::crypto::KeyError as DecodeError;

pub const PUBKEY_BLOB_LEN: usize = 260;

pub fn parse_public_key(blob: &[u8]) -> Result<RsaPublicKey, DecodeError> {
    if blob.len() != PUBKEY_BLOB_LEN {
        return Err(DecodeError::Invalid(format!(
            "RSA key blob is {} bytes, expected 260",
            blob.len()
        )));
    }
    let bits = i32::from_le_bytes(blob[0..4].try_into().unwrap());
    if bits != 1024 {
        return Err(DecodeError::Invalid(format!(
            "RSA key size {bits}, expected 1024"
        )));
    }
    let n = BigUint::from_bytes_be(&blob[4..132]);
    let e = BigUint::from_bytes_be(&blob[257..260]);
    RsaPublicKey::new(n, e).map_err(|e| DecodeError::Invalid(format!("bad RSA key: {e}")))
}

pub struct SessionKeys {
    pub aes_key: [u8; 16],
    pub xor_raw: [u8; 16],
}

impl SessionKeys {
    pub fn generate<R: RngCore + CryptoRng>(rng: &mut R) -> Self {
        let mut k = Self {
            aes_key: [0; 16],
            xor_raw: [0; 16],
        };
        rng.fill_bytes(&mut k.aes_key);
        rng.fill_bytes(&mut k.xor_raw);
        k
    }

    /// `XorKey1` as the server derives it from the first dword of the XOR blob.
    pub fn xor_key1(&self) -> u32 {
        let head = u32::from_le_bytes(self.xor_raw[0..4].try_into().unwrap());
        head.wrapping_mul(head ^ 0x15A0_2403) ^ 0x070F_1F23
    }

    /// `(rsa(aes_key), rsa(xor_raw))`, 128 bytes each.
    pub fn wrap<R: RngCore + CryptoRng>(
        &self,
        rng: &mut R,
        server_key: &RsaPublicKey,
    ) -> Result<([u8; 128], [u8; 128]), rsa::Error> {
        let enc = |data: &[u8], rng: &mut R| -> Result<[u8; 128], rsa::Error> {
            let v = server_key.encrypt(rng, Pkcs1v15Encrypt, data)?;
            Ok(v.try_into().expect("RSA-1024 ciphertext is 128 bytes"))
        };
        Ok((enc(&self.aes_key, rng)?, enc(&self.xor_raw, rng)?))
    }

    pub fn cipher(&self) -> CsCipher {
        CsCipher::new(self.aes_key, self.xor_key1())
    }
}

/// The server half of the key exchange, for fake servers in tests: an RSA-1024 key, its 260-byte
/// blob for `X2EnterWorldResponsePacket`, and the unwrap of `CSAesXorKeyPacket`.
pub struct TestServerKey {
    key: rsa::RsaPrivateKey,
}

impl TestServerKey {
    pub fn generate() -> Self {
        let mut rng = rand::thread_rng();
        Self {
            key: rsa::RsaPrivateKey::new(&mut rng, 1024).expect("RSA-1024 key"),
        }
    }

    /// `dwKeySize(1024) | modulus (128, big-endian) | 125 zero bytes | exponent (3, big-endian)`.
    pub fn blob(&self) -> Vec<u8> {
        use rsa::traits::PublicKeyParts;
        let mut blob = 1024i32.to_le_bytes().to_vec();
        blob.extend(self.key.n().to_bytes_be());
        blob.extend([0u8; 125]);
        let mut e = self.key.e().to_bytes_be();
        while e.len() < 3 {
            e.insert(0, 0);
        }
        blob.extend(e);
        blob
    }

    /// The decipher for the client's level-5 packets, from the `CSAesXorKeyPacket` body
    /// (`i32 128, i16 128, rsa(aes), rsa(xor)`).
    pub fn unwrap(&self, body: &[u8]) -> Option<super::ctos::CsDecipher> {
        let aes = self.key.decrypt(Pkcs1v15Encrypt, body.get(6..134)?).ok()?;
        let xor = self
            .key
            .decrypt(Pkcs1v15Encrypt, body.get(134..262)?)
            .ok()?;
        let keys = SessionKeys {
            aes_key: aes.try_into().ok()?,
            xor_raw: xor.try_into().ok()?,
        };
        Some(super::ctos::CsDecipher::new(keys.aes_key, keys.xor_key1()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::traits::PublicKeyParts;
    use rsa::RsaPrivateKey;

    #[test]
    fn server_can_unwrap_our_keys() {
        let mut rng = rand::thread_rng();
        let server = RsaPrivateKey::new(&mut rng, 1024).unwrap();

        // Build the blob exactly like EncryptionManager.WriteKeyParams.
        let mut blob = 1024i32.to_le_bytes().to_vec();
        blob.extend(server.n().to_bytes_be());
        blob.extend([0u8; 125]);
        let mut e = server.e().to_bytes_be();
        while e.len() < 3 {
            e.insert(0, 0);
        }
        blob.extend(e);

        let key = parse_public_key(&blob).unwrap();
        let keys = SessionKeys::generate(&mut rng);
        let (enc_aes, enc_xor) = keys.wrap(&mut rng, &key).unwrap();
        assert_eq!(
            server.decrypt(Pkcs1v15Encrypt, &enc_aes).unwrap(),
            keys.aes_key
        );
        assert_eq!(
            server.decrypt(Pkcs1v15Encrypt, &enc_xor).unwrap(),
            keys.xor_raw
        );
    }
}
