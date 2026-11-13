//! Unix SHA-256 crypt (`$5$`, Drepper), raw 32-byte output. Matches `AAEmu.Login/Core/Services/Sha256Crypt.cs`.

use sha2::{Digest, Sha256};

pub fn compute_raw(password: &str, salt: &str, rounds: u32) -> [u8; 32] {
    let pw = password.as_bytes();
    let salt = salt.as_bytes();

    let b = Sha256::new()
        .chain_update(pw)
        .chain_update(salt)
        .chain_update(pw)
        .finalize();

    let mut a = Sha256::new().chain_update(pw).chain_update(salt);
    let mut rem = pw.len();
    while rem > 0 {
        a.update(&b[..rem.min(32)]);
        rem = rem.saturating_sub(32);
    }
    let mut bits = pw.len();
    while bits > 0 {
        if bits & 1 != 0 {
            a.update(b);
        } else {
            a.update(pw);
        }
        bits >>= 1;
    }
    let mut sum_a: [u8; 32] = a.finalize().into();

    let mut dp = Sha256::new();
    for _ in 0..pw.len() {
        dp.update(pw);
    }
    let dp = dp.finalize();
    let p: Vec<u8> = (0..pw.len()).map(|i| dp[i % 32]).collect();

    let mut ds = Sha256::new();
    for _ in 0..16 + sum_a[0] as usize {
        ds.update(salt);
    }
    let ds = ds.finalize();
    let s: Vec<u8> = (0..salt.len()).map(|i| ds[i % 32]).collect();

    for c in 0..rounds {
        let mut h = Sha256::new();
        if c & 1 != 0 {
            h.update(&p);
        } else {
            h.update(sum_a);
        }
        if c % 3 != 0 {
            h.update(&s);
        }
        if c % 7 != 0 {
            h.update(&p);
        }
        if c & 1 != 0 {
            h.update(sum_a);
        } else {
            h.update(&p);
        }
        sum_a = h.finalize().into();
    }
    sum_a
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALPHABET: &[u8] = b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    const GROUPS: [(i32, usize, usize, usize); 11] = [
        (0, 10, 20, 4),
        (21, 1, 11, 4),
        (12, 22, 2, 4),
        (3, 13, 23, 4),
        (24, 4, 14, 4),
        (15, 25, 5, 4),
        (6, 16, 26, 4),
        (27, 7, 17, 4),
        (18, 28, 8, 4),
        (9, 19, 29, 4),
        (-1, 31, 30, 3),
    ];

    fn encode(raw: &[u8; 32]) -> String {
        let mut out = String::new();
        for (i2, i1, i0, n) in GROUPS {
            let b2 = if i2 >= 0 { raw[i2 as usize] as u32 } else { 0 };
            let mut w = (b2 << 16) | ((raw[i1] as u32) << 8) | raw[i0] as u32;
            for _ in 0..n {
                out.push(ALPHABET[(w & 63) as usize] as char);
                w >>= 6;
            }
        }
        out
    }

    /// Test vectors from Drepper's SHA-crypt specification.
    #[test]
    fn drepper_vectors() {
        assert_eq!(
            encode(&compute_raw("Hello world!", "saltstring", 5000)),
            "5B8vYYiY.CVt1RlTTf8KbXBH3hsxY/GNooZaBBGWEc5"
        );
        // Salt is truncated to 16 chars by the spec.
        assert_eq!(
            encode(&compute_raw("Hello world!", "saltstringsaltst", 10000)),
            "3xv.VbSHBb41AL9AvLeujZkZRBAwqFMz2.opqey6IcA"
        );
    }
}
