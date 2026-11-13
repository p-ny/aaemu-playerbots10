//! Game-channel ciphers and login challenge math.
//!
//! - [`stoc`]: server->client level-5 stream cipher (keyless, seeded by length).
//! - [`ctos`]: client->server level-5 cipher (AES-128-CBC then a sequenced XOR keystream).
//! - [`keys`]: session key generation + RSA-1024 wrapping for `CSAesXorKey`.
//! - [`sha256crypt`] / [`challenge`]: V2 login challenge (password never leaves the client).

pub mod challenge;
pub mod ctos;
pub mod keys;
pub mod sha256crypt;
pub mod stoc;

mod aes_cbc;

/// Bad server key material.
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    /// The key blob does not hold an RSA-1024 key.
    #[error("{0}")]
    Invalid(String),
}

/// Level-5 body checksum: `c = c * 0x13 + b` over every byte (truncated to u8).
pub fn crc8(data: &[u8]) -> u8 {
    data.iter()
        .fold(0u32, |c, &b| c.wrapping_mul(0x13).wrapping_add(b as u32)) as u8
}

/// Keystream step shared by both directions (client `Add` / server `Inline`).
#[inline]
fn keystream_byte(cry: &mut u32) -> u8 {
    *cry = cry.wrapping_add(0x2F_CBD5);
    let n = ((*cry >> 16) & 0xF7) as u8;
    if n == 0 {
        0xFE
    } else {
        n
    }
}

/// XORs `data` with the keystream, walking the first `stride`-aligned part backwards and the tail
/// forwards (the order the client uses; it matters because the keystream is stateful).
fn xor_keystream(data: &mut [u8], mut cry: u32, stride: usize) {
    let n = stride * (data.len() / stride);
    for i in (0..n).rev() {
        data[i] ^= keystream_byte(&mut cry);
    }
    for b in &mut data[n..] {
        *b ^= keystream_byte(&mut cry);
    }
}
