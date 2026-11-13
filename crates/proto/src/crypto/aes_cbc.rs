//! Minimal AES-CBC without padding (all protocol inputs are whole blocks).

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};

fn cbc_encrypt<C: BlockEncrypt>(c: &C, iv: [u8; 16], data: &mut [u8]) {
    assert_eq!(data.len() % 16, 0, "CBC input must be whole blocks");
    let mut prev = iv;
    for block in data.as_chunks_mut::<16>().0 {
        for (b, p) in block.iter_mut().zip(prev) {
            *b ^= p;
        }
        c.encrypt_block(GenericArray::from_mut_slice(block));
        prev = *block;
    }
}

fn cbc_decrypt<C: BlockDecrypt>(c: &C, iv: [u8; 16], data: &mut [u8]) {
    assert_eq!(data.len() % 16, 0, "CBC input must be whole blocks");
    let mut prev = iv;
    for block in data.as_chunks_mut::<16>().0 {
        let cipher = *block;
        c.decrypt_block(GenericArray::from_mut_slice(block));
        for (b, p) in block.iter_mut().zip(prev) {
            *b ^= p;
        }
        prev = cipher;
    }
}

pub fn encrypt128(key: &[u8; 16], iv: [u8; 16], data: &mut [u8]) {
    cbc_encrypt(&aes::Aes128::new(key.into()), iv, data)
}

pub fn decrypt128(key: &[u8; 16], iv: [u8; 16], data: &mut [u8]) {
    cbc_decrypt(&aes::Aes128::new(key.into()), iv, data)
}

pub fn encrypt256(key: &[u8; 32], iv: [u8; 16], data: &mut [u8]) {
    cbc_encrypt(&aes::Aes256::new(key.into()), iv, data)
}

pub fn decrypt256(key: &[u8; 32], iv: [u8; 16], data: &mut [u8]) {
    cbc_decrypt(&aes::Aes256::new(key.into()), iv, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NIST SP 800-38A F.2.1 (CBC-AES128.Encrypt), first two blocks.
    #[test]
    fn nist_vector() {
        let key: [u8; 16] = hex("2b7e151628aed2a6abf7158809cf4f3c").try_into().unwrap();
        let iv: [u8; 16] = hex("000102030405060708090a0b0c0d0e0f").try_into().unwrap();
        let mut data = hex("6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51");
        encrypt128(&key, iv, &mut data);
        assert_eq!(
            data,
            hex("7649abac8119b246cee98e9b12e9197d5086cb9b507219ee95db113a917678b2")
        );
        decrypt128(&key, iv, &mut data);
        assert_eq!(
            data,
            hex("6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51")
        );
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
}
