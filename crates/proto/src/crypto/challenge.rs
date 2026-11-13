//! V2 login challenge (`ACChallenge2` -> `CAChallengeResponse2`), inverse of the server's
//! `KoreanChallengeVerifier.VerifyV2`:
//!
//! ```text
//! K  = sha256_crypt_raw(password, salt, rounds)   // AES-256 key
//! t  = AES-256-CBC-decrypt(K, iv=0, hc)
//! t[i] += 1                                       // each u32, little-endian
//! ch = AES-256-CBC-encrypt(K, iv=0, t)
//! ```

use super::{aes_cbc, sha256crypt};

pub fn response_v2(password: &str, salt: &str, rounds: u32, hc: &[u32; 8]) -> [u32; 8] {
    let key = sha256crypt::compute_raw(password, salt, rounds);
    let mut t = [0u8; 32];
    for (chunk, v) in t.as_chunks_mut::<4>().0.iter_mut().zip(hc) {
        *chunk = v.to_le_bytes();
    }
    aes_cbc::decrypt256(&key, [0; 16], &mut t);
    for chunk in t.as_chunks_mut::<4>().0 {
        *chunk = u32::from_le_bytes(*chunk).wrapping_add(1).to_le_bytes();
    }
    aes_cbc::encrypt256(&key, [0; 16], &mut t);
    std::array::from_fn(|i| u32::from_le_bytes(t[i * 4..i * 4 + 4].try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The server verifies by recomputing the same thing from its stored hash; re-derive it the
    /// server's way (VerifyCore) and compare.
    #[test]
    fn matches_server_verification() {
        let hc = [1, 2, 3, 0xFFFF_FFFF, 5, 6, 7, 8];
        let ours = response_v2("secret", "abcdefgh12345678", 1000, &hc);

        let key = sha256crypt::compute_raw("secret", "abcdefgh12345678", 1000);
        let mut t: Vec<u8> = hc.iter().flat_map(|v| v.to_le_bytes()).collect();
        aes_cbc::decrypt256(&key, [0; 16], &mut t);
        for c in t.as_chunks_mut::<4>().0 {
            *c = u32::from_le_bytes(*c).wrapping_add(1).to_le_bytes();
        }
        aes_cbc::encrypt256(&key, [0; 16], &mut t);
        let expected: Vec<u8> = ours.iter().flat_map(|v| v.to_le_bytes()).collect();
        assert_eq!(t, expected);
    }
}
