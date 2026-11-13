//! Server->client level-5 cipher (`EncryptionManager.StoCEncrypt`). Keyless and symmetric: the same
//! call encrypts and decrypts, seeded only by the body length.

/// Decrypts (or encrypts) a level-5 S->C body in place.
pub fn apply(body: &mut [u8]) {
    let cry = (body.len() as u32) ^ 0x1F21_75A0;
    super::xor_keystream(body, cry, 4);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symmetric() {
        let plain: Vec<u8> = (0..37u8).collect();
        let mut data = plain.clone();
        apply(&mut data);
        assert_ne!(data, plain);
        apply(&mut data);
        assert_eq!(data, plain);
    }
}
