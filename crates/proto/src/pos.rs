//! Packed world positions and fixed-point coordinates.
// Vendored from AAEmu-1.2-Rust rust/crates/aaemu-commons/src/helpers.rs (GPL-3.0-or-later).

fn long_to_coord(v: i64) -> f32 {
    (v >> 32) as f32 / 4096.0
}

fn coord_to_long(v: f32) -> i64 {
    ((v * 4096.0) as i64) << 32
}

/// Bytes of a packed world position on the 10.x wire.
pub const POSITION_LEN: usize = 11;

/// Decodes the 11-byte packed world position of the 10.x protocol (x/y: 32-bit fixed point,
/// 1/512 m, sign bits in byte 10 (0x80 = x, 0x40 = y); z: 22-bit fixed point).
pub fn decode_position(v: &[u8; POSITION_LEN]) -> (f32, f32, f32) {
    let raw_x = f64::from(u32::from_le_bytes([v[0], v[1], v[2], v[3]]));
    let raw_y = f64::from(u32::from_le_bytes([v[4], v[5], v[6], v[7]]));
    let sx = if v[10] & 0x80 != 0 { -1.0 } else { 1.0 };
    let sy = if v[10] & 0x40 != 0 { -1.0 } else { 1.0 };
    let z_raw = u64::from(v[8]) | u64::from(v[9]) << 8 | u64::from(v[10] & 0x3F) << 16;
    let z = z_raw as f64 * 0.000_000_238_418_58 * 4196.0 - 100.0;
    let z = ((z * 10_000.0).round_ties_even() / 10_000.0) as f32;
    (
        (8.0 * raw_x / 4096.0 * sx) as f32,
        (8.0 * raw_y / 4096.0 * sy) as f32,
        z,
    )
}

/// Encodes a world position as the 10.x 11-byte packed form (the server's `Helpers.ConvertPosition`).
pub fn encode_position(x: f32, y: f32, z: f32) -> [u8; POSITION_LEN] {
    let pack = |l: i64| {
        let pre = l >> 31;
        ((pre ^ l.wrapping_add(pre).wrapping_add(i64::from(pre < 0))) >> 3) as u64
    };
    let rx = pack(coord_to_long(x));
    let ry = pack(coord_to_long(y));
    let rz = (f64::from((z + 100.0) / 4196.0 * 4_194_304.0) + 0.5).floor() as i64;
    let mut p = [0u8; POSITION_LEN];
    for i in 0..4 {
        p[i] = (rx >> (32 + 8 * i)) as u8;
        p[4 + i] = (ry >> (32 + 8 * i)) as u8;
    }
    p[8] = rz as u8;
    p[9] = (rz >> 8) as u8;
    p[10] = (((rz >> 16) & 0x3F) as u8) | u8::from(y < 0.0) << 6 | u8::from(x < 0.0) << 7;
    p
}

/// Fixed point (`<< 32`, 1/4096 units) to float, as used by `fx64` coordinates.
pub fn convert_long_to_coord(v: i64) -> f32 {
    long_to_coord(v)
}

/// Float to fixed point (`<< 32`, 1/4096 units), as used by `fx64` coordinates.
pub fn convert_coord_to_long(v: f32) -> i64 {
    coord_to_long(v)
}
