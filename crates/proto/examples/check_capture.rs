//! Runs every S→C parser over packets captured from the 10.x World (real client sessions) and
//! reports, per opcode the bots parse: samples, parse errors (first message) and samples the parser
//! left bytes of unread.
//!
//! Input: a flat dump of captured S→C packets, records of `u8 level (1 game, 2 proxy), u16 opcode,
//! u32 len, body` (bodies after the opcode, decrypted).
//! Usage: `cargo run -p aa-proto --example check_capture -- <frames.bin> [--all]`

use std::collections::BTreeMap;

use aa_proto::game::sc_name;
use aa_proto::sc;

#[derive(Default)]
struct Stat {
    samples: u32,
    handled: u32,
    errors: u32,
    unread: u32,
    first_error: Option<String>,
    first_unread: Option<(usize, usize)>,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("frames file");
    let all = args.any(|a| a == "--all");
    let data = std::fs::read(&path).expect("read frames");
    let mut stats: BTreeMap<(u8, u16), Stat> = BTreeMap::new();
    let mut pos = 0;
    while pos + 7 <= data.len() {
        let level = data[pos];
        let op = u16::from_le_bytes([data[pos + 1], data[pos + 2]]);
        let len = u32::from_le_bytes(data[pos + 3..pos + 7].try_into().unwrap()) as usize;
        let body = &data[pos + 7..pos + 7 + len];
        pos += 7 + len;
        let st = stats.entry((level, op)).or_default();
        st.samples += 1;
        match sc::parse(level, op, body) {
            Ok(Some(_)) => {
                st.handled += 1;
                let left = sc::last_unread();
                if left > 0 {
                    st.unread += 1;
                    st.first_unread.get_or_insert((left, len));
                }
            }
            Ok(None) => {}
            Err(e) => {
                st.handled += 1;
                st.errors += 1;
                st.first_error
                    .get_or_insert_with(|| format!("{e} (len {len})"));
            }
        }
    }
    let (mut ok, mut bad) = (0, 0);
    for ((level, op), st) in &stats {
        if st.handled == 0 && !all {
            continue;
        }
        let verdict = if st.errors == 0 && st.unread == 0 {
            "OK "
        } else {
            "BAD"
        };
        if verdict == "OK " {
            ok += 1;
        } else {
            bad += 1;
        }
        print!(
            "{verdict} {level}:0x{op:03x} {:<44} n={:<4} parsed={:<4} err={:<4} unread={:<4}",
            sc_name(*level, *op),
            st.samples,
            st.handled,
            st.errors,
            st.unread
        );
        if let Some(e) = &st.first_error {
            print!(" first_err: {e}");
        }
        if let Some((left, len)) = st.first_unread {
            print!(" first_unread: {left} of {len}");
        }
        println!();
    }
    println!("parsed opcodes: {ok} ok, {bad} bad");
}
