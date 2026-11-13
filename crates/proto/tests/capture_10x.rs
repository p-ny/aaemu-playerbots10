//! Every S→C parser against packets captured from the 10.x World in real-client sessions: no
//! parse errors and no unread bytes for any packet the bots read.
//!
//! A capture holds player data, so none is part of the repository: set `AA_CAPTURE_FRAMES`
//! to a flat dump of captured S→C packets (records of `u8 level, u16 opcode, u32 len, body`, the
//! format `examples/check_capture.rs` reads).
//! Without it the test is skipped.

use aa_proto::game::sc_name;
use aa_proto::sc;

#[test]
fn captured_10x_packets_parse_exactly() {
    let Some(path) = std::env::var_os("AA_CAPTURE_FRAMES") else {
        eprintln!("AA_CAPTURE_FRAMES not set: skipped");
        return;
    };
    let data = std::fs::read(path).expect("read the capture dump");
    let (mut parsed, mut bad) = (0usize, Vec::new());
    let mut pos = 0;
    while pos + 7 <= data.len() {
        let level = data[pos];
        let op = u16::from_le_bytes([data[pos + 1], data[pos + 2]]);
        let len = u32::from_le_bytes(data[pos + 3..pos + 7].try_into().unwrap()) as usize;
        let body = &data[pos + 7..pos + 7 + len];
        pos += 7 + len;
        match sc::parse(level, op, body) {
            Ok(Some(_)) if sc::last_unread() == 0 => parsed += 1,
            Ok(Some(_)) => bad.push(format!(
                "{} left {} of {len} bytes",
                sc_name(level, op),
                sc::last_unread()
            )),
            Ok(None) => {}
            Err(e) => bad.push(format!("{}: {e}", sc_name(level, op))),
        }
    }
    bad.sort();
    bad.dedup();
    assert!(
        bad.is_empty(),
        "{parsed} parsed; failures:\n{}",
        bad.join("\n")
    );
    assert!(parsed > 0, "the dump holds no packet the bots parse");
}
