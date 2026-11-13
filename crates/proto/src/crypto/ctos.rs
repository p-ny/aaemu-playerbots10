//! Client->server level-5 cipher: the inverse of AAEmu's `EncryptionManager.CSDecrypt`.
//!
//! Plaintext `[crc8][count][type u16][body]` is zero-padded to 16-byte blocks, AES-128-CBC encrypted
//! (IV = last cipher block of the previous packet, zero for the first), then XORed with a keystream
//! derived from the session XOR key, the real plaintext length and a running sequence. The wire body is
//! `[hash][cipher]`, where `hash = 0x30 + (len - 1) % 16` lets the server recover the real length.

use super::{aes_cbc, xor_keystream};

pub struct CsCipher {
    aes_key: [u8; 16],
    /// `XorKey1` squared, the form the keystream actually uses.
    xor_key_sq: u32,
    iv: [u8; 16],
    seq: u8,
    m_seq: u32,
}

impl CsCipher {
    pub fn new(aes_key: [u8; 16], xor_key1: u32) -> Self {
        Self {
            aes_key,
            xor_key_sq: xor_key1.wrapping_mul(xor_key1),
            iv: [0; 16],
            seq: 0,
            m_seq: 0,
        }
    }

    /// Encrypts one plaintext game packet. Returns `[hash][cipher]`. Must be called in send order.
    pub fn encrypt(&mut self, plain: &[u8]) -> Vec<u8> {
        assert!(!plain.is_empty(), "level-5 plaintext is never empty");
        let len = plain.len();
        let blocks = len.div_ceil(16);

        let mut out = Vec::with_capacity(1 + blocks * 16);
        out.push(0x30 + ((len - 1) % 16) as u8);
        out.extend_from_slice(plain);
        out.resize(1 + blocks * 16, 0);

        let cipher = &mut out[1..];
        aes_cbc::encrypt128(&self.aes_key, self.iv, cipher);
        self.iv.copy_from_slice(&cipher[cipher.len() - 16..]);

        let cry = (len as u32).wrapping_mul(self.xor_key_sq)
            ^ (self.make_seq() as u32).wrapping_add(0x75A0_2419)
            ^ 0x68BE_F515;
        xor_keystream(cipher, cry, seq_offset(self.seq));
        self.seq = self.seq.wrapping_add(self.make_seq()).wrapping_add(1);
        out
    }

    fn make_seq(&mut self) -> u8 {
        self.m_seq = self.m_seq.wrapping_add(0x2F_A245);
        let r = ((self.m_seq >> 14) & 0x73) as u8;
        if r == 0 {
            0xFE
        } else {
            r
        }
    }
}

/// The server side of the C->S cipher: a straight port of AAEmu's `EncryptionManager.CSDecrypt`
/// (`CsDecodeXor`, `CsDecodeAes`), written against the C# rather than against [`CsCipher`]. For fake
/// servers in tests.
pub struct CsDecipher {
    aes_key: [u8; 16],
    xor_key1: u32,
    iv: [u8; 16],
    cs_seq: u8,
    cs_m_seq: u32,
}

impl CsDecipher {
    pub fn new(aes_key: [u8; 16], xor_key1: u32) -> Self {
        Self {
            aes_key,
            xor_key1,
            iv: [0; 16],
            cs_seq: 0,
            cs_m_seq: 0,
        }
    }

    fn make_seq(&mut self) -> u8 {
        self.cs_m_seq = self.cs_m_seq.wrapping_add(0x2FA245);
        let r = ((self.cs_m_seq >> 14) & 0x73) as u8;
        if r == 0 {
            0xFE
        } else {
            r
        }
    }

    /// `input` = the frame payload `[0xDD][5][hash][cipher]`; returns `[crc8][count][type u16][body]`.
    pub fn decrypt(&mut self, input: &[u8]) -> Vec<u8> {
        let mut hash_map = [0u32; 256];
        for i in 0..16 {
            hash_map[0x30 + i] = i as u32 + 1;
        }
        let body = &input[3..];
        let msg_key = (((input.len() / 16) as u32 - 1) << 4) + hash_map[input[2] as usize];
        let xor_key = self.xor_key1.wrapping_mul(self.xor_key1);
        let mul = msg_key.wrapping_mul(xor_key);
        let cry = mul ^ (self.make_seq() as u32).wrapping_add(0x75A02419) ^ 0x68BEF515;
        let offset = seq_offset(self.cs_seq);
        let mut xored = body.to_vec();
        xor_keystream(&mut xored, cry, offset);
        let next = self.make_seq();
        self.cs_seq = self.cs_seq.wrapping_add(next).wrapping_add(1);

        let iv = self.iv;
        self.iv.copy_from_slice(&xored[xored.len() - 16..]);
        aes_cbc::decrypt128(&self.aes_key, iv, &mut xored);
        xored.truncate(msg_key as usize);
        xored
    }
}

fn seq_offset(seq: u8) -> usize {
    match seq {
        0 => 9,
        s if s % 3 == 0 => 5,
        s if s % 5 == 0 => 2,
        s if s % 7 == 0 => 11,
        s if s % 9 == 0 => 3,
        s if s % 11 == 0 => 7,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_decrypts_what_we_encrypt() {
        let aes_key = *b"0123456789abcdef";
        let xor_key1 = 0xA1B2_C3D4;
        let mut client = CsCipher::new(aes_key, xor_key1);
        let mut server = CsDecipher::new(aes_key, xor_key1);

        // Many lengths, including exact block multiples, across enough packets to exercise every stride.
        for (i, len) in (1..=70).chain([16, 32, 48, 200, 1000]).enumerate() {
            let plain: Vec<u8> = (0..len).map(|b| (b * 7 + i) as u8).collect();
            let body = client.encrypt(&plain);
            let mut input = vec![0xDD, 5];
            input.extend_from_slice(&body);
            assert_eq!(server.decrypt(&input), plain, "packet #{i} len {len}");
        }
    }
}
