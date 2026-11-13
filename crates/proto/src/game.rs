//! Game server connection for the 10.0.2.13 protocol (AAEmu 10.x World).
//!
//! Derived from the 1.2 connection of this crate and reshaped for hundreds of bots in one process:
//! the connection is a plain struct owned by one task, there is no reader task and no channel.
//!
//! # Wire format
//!
//! Every frame is `[u16 len][payload]` ([`crate::codec::FrameCodec`]) and every payload starts
//! `0xDD [level]`.
//!
//! | dir | level | payload |
//! | --- | --- | --- |
//! | C→S | 1 game | `DD 01 00 00 <u16 opcode> <body>`, only before the key exchange |
//! | C→S | 5 game | `DD 05 <ctos cipher of [crc8][count][u16 opcode][body]>`, after `CSAesXorKey` |
//! | C→S | 2 proxy | `DD 02 <u16 opcode> <body>` (never encrypted) |
//! | S→C | 1 game | `DD 01 <crc> <count> <u16 opcode> <body>` |
//! | S→C | 5 game | `DD 05 <stoc cipher of [crc8][count][u16 opcode][body]>` |
//! | S→C | 2 proxy | `DD 02 <u16 opcode> <body>` |
//! | S→C | 4 batch | `DD 04 <u16 count> <raw deflate>`; the 10.x World never sends it, kept for safety |
//!
//! Level-5 packets are delivered as level [`LEVEL_GAME`], so callers see one game level.

use std::collections::VecDeque;
use std::fmt::Display;
use std::io;
use std::sync::OnceLock;

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite, ReadHalf, WriteHalf};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpStream, ToSocketAddrs};
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::codec::{FrameCodec, Parts};
use crate::crypto::{self, ctos::CsCipher};
use crate::cs::Out;
use crate::opcodes::{pp, sc};

/// First byte of every game payload.
pub const FRAME_MAGIC: u8 = 0xDD;
/// Level 1: ordinary game packets (`CS*` / `SC*`).
pub const LEVEL_GAME: u8 = 1;
/// Level 2: the proxy (CryNetwork) channel (`Ping`, `FinishState`, ...).
pub const LEVEL_PROXY: u8 = 2;
/// Level 4: `CompressedGamePackets`, a raw-deflate batch of level-1 packets.
pub const LEVEL_COMPRESSED: u8 = 4;
/// Level 5: an encrypted game packet (both directions after the key exchange).
pub const LEVEL_ENCRYPTED: u8 = 5;
/// The opcode of a packet that has no 10.x opcode ([`crate::opcodes`] marks those `0xfff`).
pub const UNKNOWN_OPCODE: u16 = 0xfff;
/// Largest frame payload the transport can carry (`u16` length).
pub const MAX_FRAME: usize = u16::MAX as usize;
/// Hard cap on an inflated batch.
pub const MAX_BATCH_BYTES: usize = 1024 * 1024;
/// Hard cap on the entries of one batch (the native all-skill cooldown reset needs ~4k).
pub const MAX_BATCH_ENTRIES: usize = 4096;

/// Why a game connection call failed.
#[derive(Debug, thiserror::Error)]
pub enum GameError {
    /// The TCP connection could not be opened.
    #[error("connecting to game server {addr}: {source}")]
    Connect {
        /// The address that was dialled.
        addr: String,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The socket failed after it was open.
    #[error("game socket error: {0}")]
    Io(#[from] io::Error),
    /// The server closed the connection.
    #[error("game server closed the connection")]
    Closed,
    /// A frame that is not a game frame (wrong magic or too short): the stream is desynchronised.
    #[error("invalid game frame: {0}")]
    BadFrame(&'static str),
    /// A packet whose header and body do not fit one frame.
    #[error("packet of {len} bytes does not fit a frame")]
    TooLarge {
        /// Header plus body length.
        len: usize,
    },
}

/// Result of a game connection call.
pub type Result<T> = std::result::Result<T, GameError>;

// ---- opcode names ---------------------------------------------------------------------------

const OPCODE_SPACE: usize = 0x1000;

fn name_lut(
    cell: &'static OnceLock<Box<[&'static str]>>,
    table: &'static [(u16, &'static str)],
) -> &'static [&'static str] {
    cell.get_or_init(|| {
        let mut names = vec![""; OPCODE_SPACE];
        for &(opcode, name) in table {
            // First wins, like `opcodes::name_of` (several 1.2 packets share the 0xfff marker).
            if let Some(slot) = names.get_mut(usize::from(opcode)) {
                if slot.is_empty() {
                    *slot = name;
                }
            }
        }
        names.into_boxed_slice()
    })
}

fn lookup(lut: &[&'static str], opcode: u16) -> &'static str {
    match lut.get(usize::from(opcode)) {
        Some(name) if !name.is_empty() => name,
        _ => "Unknown",
    }
}

/// C# constant name of an S→C opcode (`level` 1 = `SC*`, 2 = proxy); `"Unknown"` when unnamed.
/// O(1): the table is expanded once into a direct-indexed lookup.
pub fn sc_name(level: u8, opcode: u16) -> &'static str {
    static SC: OnceLock<Box<[&'static str]>> = OnceLock::new();
    static PP: OnceLock<Box<[&'static str]>> = OnceLock::new();
    if level == LEVEL_PROXY {
        lookup(name_lut(&PP, pp::NAMES), opcode)
    } else {
        lookup(name_lut(&SC, sc::NAMES), opcode)
    }
}

/// C# constant name of a C→S opcode (`level` 1 = `CS*`, 2 = proxy); `"Unknown"` when unnamed.
pub fn cs_name(level: u8, opcode: u16) -> &'static str {
    static CS: OnceLock<Box<[&'static str]>> = OnceLock::new();
    static PP: OnceLock<Box<[&'static str]>> = OnceLock::new();
    if level == LEVEL_PROXY {
        lookup(name_lut(&PP, pp::NAMES), opcode)
    } else {
        lookup(name_lut(&CS, crate::opcodes::cs::NAMES), opcode)
    }
}

// ---- packets and stats ----------------------------------------------------------------------

/// A received S→C packet.
///
/// `body` is a zero-copy view of the frame (or inflated batch) it arrived in. Parse it and drop
/// the packet: keeping it for long pins that buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    /// 1 = game (`SC*`), 2 = proxy.
    pub level: u8,
    /// The `sc::` (level 1) or `pp::` (level 2) opcode.
    pub opcode: u16,
    /// The packet body, without the `DD <level> ...` header.
    pub body: Bytes,
}

impl Packet {
    /// C# constant name of the opcode, for logging. O(1).
    pub fn name(&self) -> &'static str {
        sc_name(self.level, self.opcode)
    }

    /// Whether this is the packet `opcode` of `level`.
    pub fn is(&self, level: u8, opcode: u16) -> bool {
        self.level == level && self.opcode == opcode
    }

    /// Whether this is the level-1 packet `opcode` (an `sc::` constant).
    pub fn is_sc(&self, opcode: u16) -> bool {
        self.is(LEVEL_GAME, opcode)
    }
}

/// Counters of one connection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConnStats {
    /// Packets handed out by [`GameConn::recv`] (batch entries count individually).
    pub packets_in: u64,
    /// Frames queued by [`GameConn::send`].
    pub packets_out: u64,
    /// Bytes read from the socket, including the 2-byte frame lengths.
    pub bytes_in: u64,
    /// Bytes queued for the socket, including the 2-byte frame lengths.
    pub bytes_out: u64,
    /// Level-4 batch frames received.
    pub batches: u64,
    /// Batch entries lost: after an entry the splitter cannot size, or with a batch that failed
    /// to inflate or exceeded a cap.
    pub batch_entries_dropped: u64,
}

// ---- the connection -------------------------------------------------------------------------

/// A game server connection, driven by one task.
///
/// Both socket halves live inside; `recv` and `send` take `&mut self`, so a bot's task
/// alternates between them in a `tokio::select!` over `recv()`, its tick timer and its command
/// channel. Generic over the transport so tests can run it on in-memory pipes; the default is
/// the owned halves of a [`TcpStream`].
pub struct GameConn<R = OwnedReadHalf, W = OwnedWriteHalf> {
    rd: FramedRead<R, FrameCodec>,
    wr: FramedWrite<W, FrameCodec>,
    /// Entries of an already-expanded batch, delivered before the socket is read again.
    pending: VecDeque<Packet>,
    count: u8,
    stats: ConnStats,
    /// The C→S cipher, once `CSAesXorKey` is sent.
    cipher: Option<CsCipher>,
    /// Fake servers in tests: decrypts the client's level-5 packets.
    server_decipher: Option<crate::crypto::ctos::CsDecipher>,
}

impl GameConn {
    /// Connects to the game server at `addr` (`host:port`, or any `ToSocketAddrs`) with
    /// `TCP_NODELAY`.
    pub async fn connect(addr: impl ToSocketAddrs + Display) -> Result<Self> {
        let sock = TcpStream::connect(&addr)
            .await
            .map_err(|source| GameError::Connect {
                addr: addr.to_string(),
                source,
            })?;
        sock.set_nodelay(true)?;
        let (rd, wr) = sock.into_split();
        Ok(Self::new(rd, wr))
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> GameConn<ReadHalf<T>, WriteHalf<T>> {
    /// A connection over any duplex transport (in-memory pipes in tests).
    pub fn from_io(io: T) -> Self {
        let (rd, wr) = tokio::io::split(io);
        Self::new(rd, wr)
    }
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> GameConn<R, W> {
    /// A connection over the two halves of a transport.
    pub fn new(rd: R, wr: W) -> Self {
        Self {
            rd: FramedRead::with_capacity(rd, FrameCodec, 4096),
            wr: FramedWrite::new(wr, FrameCodec),
            pending: VecDeque::new(),
            count: 0,
            stats: ConnStats::default(),
            cipher: None,
            server_decipher: None,
        }
    }

    /// Fake servers in tests: from now on level-5 frames are the client's, decrypted with `d`.
    pub fn accept_client_cipher(&mut self, d: crate::crypto::ctos::CsDecipher) {
        self.server_decipher = Some(d);
    }

    /// From now on level-1 packets are sent encrypted (level 5). Call right after sending
    /// `CSAesXorKey` with the keys it carried.
    pub fn enable_encryption(&mut self, cipher: CsCipher) {
        self.cipher = Some(cipher);
        self.count = 0;
    }

    /// Whether game packets are sent encrypted.
    pub fn encrypted(&self) -> bool {
        self.cipher.is_some()
    }

    /// The connection's counters.
    pub fn stats(&self) -> ConnStats {
        self.stats
    }

    /// Queues one packet and flushes it.
    ///
    /// Level 1 is sent encrypted as level 5 once [`GameConn::enable_encryption`] was called, as
    /// plain `DD 01 00 00 <op> <body>` before; any other level (2 = proxy) as
    /// `DD <level> <op> <body>`. Cancelling the future after the frame is queued is harmless: the
    /// frame is flushed by the next `send`/[`GameConn::flush`].
    pub async fn send(&mut self, out: &Out) -> Result<()> {
        if out.level == LEVEL_GAME && out.opcode == UNKNOWN_OPCODE {
            // A packet the 10.x client does not have: never put it on the wire.
            tracing::debug!(packet = out.name(), "not sent: no 10.x opcode");
            return Ok(());
        }
        let [op_lo, op_hi] = out.opcode.to_le_bytes();
        let frame: Vec<u8> = match (&mut self.cipher, out.level) {
            (Some(cipher), LEVEL_GAME) => {
                let mut plain = Vec::with_capacity(4 + out.body.len());
                plain.push(0);
                plain.push(self.count);
                plain.extend_from_slice(&[op_lo, op_hi]);
                plain.extend_from_slice(&out.body);
                plain[0] = crypto::crc8(&plain[1..]);
                self.count = self.count.wrapping_add(1);
                let mut f = vec![FRAME_MAGIC, LEVEL_ENCRYPTED];
                f.extend(cipher.encrypt(&plain));
                f
            }
            (None, LEVEL_GAME) => {
                let mut f = vec![FRAME_MAGIC, LEVEL_GAME, 0, 0, op_lo, op_hi];
                f.extend_from_slice(&out.body);
                f
            }
            (_, level) => {
                let mut f = vec![FRAME_MAGIC, level, op_lo, op_hi];
                f.extend_from_slice(&out.body);
                f
            }
        };
        let len = frame.len();
        if len > MAX_FRAME {
            return Err(GameError::TooLarge { len });
        }
        self.wr
            .feed(Parts {
                head: &frame,
                body: &[],
            })
            .await?;
        self.stats.packets_out += 1;
        self.stats.bytes_out += len as u64 + 2;
        self.flush().await
    }

    /// Flushes queued frames to the socket.
    pub async fn flush(&mut self) -> Result<()> {
        SinkExt::<Parts<'static>>::flush(&mut self.wr).await?;
        Ok(())
    }

    /// Shuts the write half down (the server sees EOF). Does not send `CSLeaveWorldPacket`.
    pub async fn close(&mut self) -> Result<()> {
        SinkExt::<Parts<'static>>::close(&mut self.wr).await?;
        Ok(())
    }

    /// The next S→C packet, with level-4 batches expanded in order.
    ///
    /// **Cancel-safe**: the only suspension point is the socket read, and partial frames stay
    /// in the reader's buffer, so dropping the future (a `select!` branch losing) never loses
    /// or reorders packets. Entries of a batch already expanded are returned first, without
    /// touching the socket.
    ///
    /// Returns [`GameError::Closed`] on EOF. A malformed batch is not an error (it is counted
    /// in [`ConnStats::batch_entries_dropped`]); a frame that is not a game frame is.
    pub async fn recv(&mut self) -> Result<Packet> {
        loop {
            if let Some(packet) = self.pending.pop_front() {
                self.stats.packets_in += 1;
                return Ok(packet);
            }
            let frame = match self.rd.next().await {
                Some(Ok(frame)) => frame,
                Some(Err(e)) => return Err(GameError::Io(e)),
                None => return Err(GameError::Closed),
            };
            self.stats.bytes_in += frame.len() as u64 + 2;
            self.ingest(frame)?;
        }
    }

    /// Decodes one frame into `pending` (synchronous, so `recv` stays cancel-safe).
    fn ingest(&mut self, frame: Bytes) -> Result<()> {
        if frame.len() < 4 || frame[0] != FRAME_MAGIC {
            return Err(GameError::BadFrame("missing 0xDD header"));
        }
        match frame[1] {
            LEVEL_GAME => {
                if frame.len() < 6 {
                    return Err(GameError::BadFrame("short level-1 frame"));
                }
                self.pending.push_back(Packet {
                    level: LEVEL_GAME,
                    opcode: u16::from_le_bytes([frame[4], frame[5]]),
                    body: frame.slice(6..),
                });
            }
            LEVEL_ENCRYPTED => {
                let mut data = match &mut self.server_decipher {
                    Some(d) => d.decrypt(&frame),
                    None => {
                        let mut data = frame[2..].to_vec();
                        crypto::stoc::apply(&mut data);
                        data
                    }
                };
                if data.len() < 4 {
                    return Err(GameError::BadFrame("short level-5 frame"));
                }
                if data[0] != crypto::crc8(&data[1..]) {
                    return Err(GameError::BadFrame("level-5 checksum mismatch"));
                }
                let opcode = u16::from_le_bytes([data[2], data[3]]);
                let body = Bytes::from(data).slice(4..);
                self.pending.push_back(Packet {
                    level: LEVEL_GAME,
                    opcode,
                    body,
                });
            }
            LEVEL_PROXY => self.pending.push_back(Packet {
                level: LEVEL_PROXY,
                opcode: u16::from_le_bytes([frame[2], frame[3]]),
                body: frame.slice(4..),
            }),
            LEVEL_COMPRESSED => {
                let count = u16::from_le_bytes([frame[2], frame[3]]);
                self.stats.batches += 1;
                self.expand_batch(count, &frame[4..]);
            }
            level => tracing::debug!(level, "ignoring a game frame of an unknown level"),
        }
        Ok(())
    }

    fn expand_batch(&mut self, count: u16, deflated: &[u8]) {
        let raw = match inflate_batch(deflated) {
            Ok(raw) => Bytes::from(raw),
            Err(e) => {
                tracing::warn!(entries = count, error = %e, "dropping an undecodable DD04 batch");
                self.stats.batch_entries_dropped += u64::from(count);
                return;
            }
        };
        let before = self.pending.len();
        let outcome = split_batch(&raw, count, &mut self.pending);
        let delivered = self.pending.len() - before;
        if delivered < usize::from(count) {
            let dropped = usize::from(count) - delivered;
            self.stats.batch_entries_dropped += dropped as u64;
            tracing::debug!(
                delivered,
                dropped,
                reason = ?outcome,
                "DD04 batch ended early"
            );
        }
    }
}

// ---- level 4 batches ------------------------------------------------------------------------

/// Why a batch could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BatchError {
    /// The deflate stream is corrupt.
    #[error("invalid deflate stream")]
    Inflate,
    /// The inflated payload exceeds [`MAX_BATCH_BYTES`].
    #[error("batch exceeds {MAX_BATCH_BYTES} bytes")]
    TooLarge,
    /// The entry count exceeds [`MAX_BATCH_ENTRIES`].
    #[error("batch exceeds {MAX_BATCH_ENTRIES} entries")]
    TooManyEntries,
    /// An entry header with a non-zero first `u16`, or too few bytes for the declared entries.
    #[error("invalid batch structure")]
    Structure,
    /// An entry whose opcode has no known length.
    #[error("batched opcode 0x{0:03x} has no known length")]
    Unsupported(u16),
}

/// Inflates a raw-deflate batch payload into a growable buffer capped at [`MAX_BATCH_BYTES`]
/// (the buffer starts at twice the input and doubles, it is not preallocated at the cap).
pub fn inflate_batch(deflated: &[u8]) -> std::result::Result<Vec<u8>, BatchError> {
    use miniz_oxide::inflate::{decompress_to_vec_with_limit, TINFLStatus};
    decompress_to_vec_with_limit(deflated, MAX_BATCH_BYTES).map_err(|e| {
        if e.status == TINFLStatus::HasMoreOutput {
            BatchError::TooLarge
        } else {
            BatchError::Inflate
        }
    })
}

/// Splits an inflated batch (`count` entries of `[u16 0][u16 opcode][body]`) into level-1
/// packets pushed onto `out`, sharing `raw` without copying. Stops at the first entry that is
/// malformed or cannot be sized; the entries before it stay pushed and the reason is returned.
pub fn split_batch(
    raw: &Bytes,
    count: u16,
    out: &mut VecDeque<Packet>,
) -> std::result::Result<(), BatchError> {
    if usize::from(count) > MAX_BATCH_ENTRIES {
        return Err(BatchError::TooManyEntries);
    }
    let mut pos = 0usize;
    for _ in 0..count {
        let rest = &raw[pos..];
        if rest.len() < 4 || rest[0] != 0 || rest[1] != 0 {
            return Err(BatchError::Structure);
        }
        let opcode = u16::from_le_bytes([rest[2], rest[3]]);
        let len = match batch_entry_len(opcode, &rest[4..]) {
            Ok(len) => len,
            Err(SizeError::Unsupported) => return Err(BatchError::Unsupported(opcode)),
            Err(SizeError::Truncated | SizeError::Invalid) => return Err(BatchError::Structure),
        };
        out.push_back(Packet {
            level: LEVEL_GAME,
            opcode,
            body: raw.slice(pos + 4..pos + 4 + len),
        });
        pos += 4 + len;
    }
    Ok(())
}

/// Why [`batch_entry_len`] could not size an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeError {
    /// The entry runs past the end of the batch.
    Truncated,
    /// A field holds a value the layout does not allow (unknown caster / target type, ...).
    Invalid,
    /// The opcode has no known length.
    Unsupported,
}

/// Byte length of the body of the batched `sc::` packet `opcode` that starts `body[0]`.
///
/// Splittable opcodes: every packet this server batches (`SCUnitDamagedPacket`,
/// `SCUnitHealedPacket`, `SCPlotEventPacket`, `SCSkillCooldownResetPacket`) and, because the
/// native format batches them too, `SCSkillStartedPacket`, `SCSkillFiredPacket`,
/// `SCSkillEndedPacket`, `SCSkillStoppedPacket`, `SCCastingStoppedPacket`, `SCPlotEndedPacket`,
/// `SCBuffCreatedPacket`, `SCBuffRemovedPacket`, `SCBuffLearnedPacket` and `SCErrorMsgPacket`.
/// Layouts: `aaemu-game/src/systems/skills/packets.rs` (`Marshal` impls) and
/// `systems/skills/types.rs` (`SkillCaster`, `SkillCastTarget`, `SkillObject`, `CastAction`,
/// `PlotObject` writers); the lengths agree with the test client's `batch_body`.
pub fn batch_entry_len(opcode: u16, body: &[u8]) -> std::result::Result<usize, SizeError> {
    let mut c = Cursor { b: body, pos: 0 };
    match opcode {
        // bc obj, u32 skill, u32 tag, bool gcd
        sc::SCSkillCooldownResetPacket => c.skip(12)?,
        // u16 tl
        sc::SCSkillEndedPacket | sc::SCPlotEndedPacket => c.skip(2)?,
        // u16 tl, u32 duration
        sc::SCCastingStoppedPacket => c.skip(6)?,
        // bc obj + u32 index / buff id / skill id
        sc::SCBuffRemovedPacket | sc::SCBuffLearnedPacket | sc::SCSkillStoppedPacket => {
            c.skip(7)?
        }
        // i16, i16, u32, bool
        sc::SCErrorMsgPacket => c.skip(9)?,
        sc::SCUnitDamagedPacket => {
            c.cast_action()?;
            c.caster()?;
            c.skip(7)?; // bc caster, bc target, u8 crime state
            c.pisc(3)?; // damage, absorbed, 0
            c.pisc(3)?; // 0, 0, mana burn
            c.skip(5)?; // u8 holdable, u16 hit type, u8 flag, u8 result
        }
        sc::SCUnitHealedPacket => {
            c.cast_action()?;
            c.caster()?;
            c.skip(14)?; // bc target, u8 type, u8 hit type, i32 value, i32 0, u8 result
        }
        sc::SCPlotEventPacket => {
            c.skip(10)?; // u16 tl, u32 event, u32 skill
            c.plot_object()?;
            c.plot_object()?;
            c.skip(18)?; // u64 item, bc obj, u16 casting time, bc 0, i16 0
            let units = usize::from(c.u8()?);
            c.skip(units * 3)?; // bc per target unit
            if c.u8()? & 8 != 0 {
                c.skip(13 * 4)?;
            }
        }
        sc::SCBuffCreatedPacket => {
            c.caster()?;
            c.skip(22)?; // u32 caster, bc owner, u32 index, u32 buff, u8 level, i16, u32 skill
            c.pisc(4)?; // charge, duration, 0, tick
        }
        sc::SCSkillStartedPacket | sc::SCSkillFiredPacket => {
            c.skip(6)?; // u32 skill, u16 tl
            c.caster()?;
            c.target()?;
            c.skill_object()?;
            if opcode == sc::SCSkillFiredPacket {
                c.skip(10)?; // i16, i16, u8, u32 fire anim, u8
            } else {
                c.skip(5)?; // u16 real cast, u16 base cast, u8 synergy
                let extra = c.u8()?;
                if extra & !7 != 0 {
                    return Err(SizeError::Invalid);
                }
                if extra & 1 != 0 {
                    c.skip(1)?;
                }
                if extra & 2 != 0 {
                    c.skip(2)?;
                }
                if extra & 4 != 0 {
                    c.skip(4)?;
                }
            }
        }
        _ => return Err(SizeError::Unsupported),
    }
    Ok(c.pos)
}

/// A bounds-checked read position over a batch entry, for sizing.
struct Cursor<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn skip(&mut self, n: usize) -> std::result::Result<(), SizeError> {
        let end = self.pos.checked_add(n).ok_or(SizeError::Truncated)?;
        if end > self.b.len() {
            return Err(SizeError::Truncated);
        }
        self.pos = end;
        Ok(())
    }

    fn u8(&mut self) -> std::result::Result<u8, SizeError> {
        let v = *self.b.get(self.pos).ok_or(SizeError::Truncated)?;
        self.pos += 1;
        Ok(v)
    }

    /// `PacketStream::read_string`: an `i16` length (negative = 0) and that many bytes.
    fn string(&mut self) -> std::result::Result<(), SizeError> {
        let lo = self.u8()?;
        let hi = self.u8()?;
        let len = i16::from_le_bytes([lo, hi]).max(0) as usize;
        self.skip(len)
    }

    /// `write_pisc` of `count` (<= 4) values: a header byte with two width bits per value.
    fn pisc(&mut self, count: usize) -> std::result::Result<(), SizeError> {
        let header = self.u8()?;
        for i in 0..count {
            self.skip(match (header >> (i * 2)) & 3 {
                0 => 1,
                1 => 2,
                2 => 3,
                _ => 4,
            })?;
        }
        Ok(())
    }

    /// `CastAction.Write`: a type byte then 6 / 14 / 13 / 11 / 7 bytes.
    fn cast_action(&mut self) -> std::result::Result<(), SizeError> {
        match self.u8()? {
            0 => self.skip(6),
            1 => self.skip(14),
            2 => self.skip(13),
            3 => self.skip(11),
            4 => self.skip(7),
            _ => Err(SizeError::Invalid),
        }
    }

    /// `SkillCaster.Write`: a type byte then bc (+ item or mount fields).
    fn caster(&mut self) -> std::result::Result<(), SizeError> {
        match self.u8()? {
            0 | 1 | 4 => self.skip(3),
            2 => self.skip(3 + 8 + 4 + 1 + 4),
            3 => self.skip(3 + 4),
            _ => Err(SizeError::Invalid),
        }
    }

    /// `SkillCastTarget.Write`.
    fn target(&mut self) -> std::result::Result<(), SizeError> {
        match self.u8()? {
            0 | 4 => self.skip(3),
            1 => self.skip(8 + 8 + 4 + 4 + 3 + 3),
            2 => self.skip(8 + 8 + 4 + 12 + 12),
            3 => self.skip(3 + 8 + 4 + 1),
            5 => self.skip(8 + 8 + 4 + 4),
            _ => Err(SizeError::Invalid),
        }
    }

    /// `SkillObject.Write`.
    fn skill_object(&mut self) -> std::result::Result<(), SizeError> {
        match self.u8()? {
            0 => Ok(()),
            1 => self.skip(1 + 4 + 8 + 8 + 4),
            2 => {
                self.skip(4)?;
                self.string()
            }
            3 | 6 => self.string(),
            4 => self.skip(8 + 8 + 4),
            5 => self.skip(4),
            7 => self.skip(4 + 8 + 1),
            _ => Err(SizeError::Invalid),
        }
    }

    /// `PlotObject.Write`: type 1 = bc, type 2 = packed position + 3 rotation bytes.
    fn plot_object(&mut self) -> std::result::Result<(), SizeError> {
        match self.u8()? {
            1 => self.skip(3),
            2 => self.skip(12),
            _ => Err(SizeError::Invalid),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::PacketStream;

    #[test]
    fn names_are_looked_up_in_constant_time_tables() {
        assert_eq!(
            sc_name(LEVEL_GAME, sc::SCUnitDeathPacket),
            "SCUnitDeathPacket"
        );
        assert_eq!(sc_name(LEVEL_PROXY, pp::PongPacket), "PongPacket");
        assert_eq!(
            cs_name(LEVEL_GAME, crate::opcodes::cs::CSMoveUnitPacket),
            "CSMoveUnitPacket"
        );
        assert_eq!(cs_name(LEVEL_PROXY, pp::PingPacket), "PingPacket");
        assert_eq!(sc_name(LEVEL_GAME, 0xFFE), "Unknown");
        assert_eq!(sc_name(LEVEL_GAME, 0xFFFF), "Unknown");
        let p = Packet {
            level: LEVEL_GAME,
            opcode: sc::SCUnitDeathPacket,
            body: Bytes::new(),
        };
        assert_eq!(p.name(), "SCUnitDeathPacket");
        assert!(p.is_sc(sc::SCUnitDeathPacket));
        assert!(!p.is(LEVEL_PROXY, sc::SCUnitDeathPacket));
    }

    /// The `Marshal` output of the server's packets, rebuilt by hand, must be sized exactly.
    #[test]
    #[ignore = "1.2 DD04 batch layout; the 10.x World never sends DD04"]
    fn sizes_the_batched_server_packets() {
        // SCUnitDamagedPacket: cast action Skill, caster Unit, ids, pisc x2, tail.
        let mut s = PacketStream::new();
        s.write_u8(0).write_u32(1234).write_u16(7); // CastAction::Skill
        s.write_u8(0).write_bc(0x010203); // SkillCaster::Unit
        s.write_bc(0x010203).write_bc(0x0405).write_u8(0);
        s.write_pisc(&[100_000, 3, 0]); // damage needs a bc; the header byte sets the widths
        s.write_pisc(&[0, 0, 70_000]);
        s.write_u8(2).write_u16(288 | 1).write_u8(1).write_u8(1);
        assert_eq!(
            batch_entry_len(sc::SCUnitDamagedPacket, s.as_bytes()),
            Ok(s.len())
        );
        // The same body followed by the next entry's bytes is still sized exactly.
        let mut longer = s.as_bytes().to_vec();
        longer.extend_from_slice(&[9; 20]);
        assert_eq!(
            batch_entry_len(sc::SCUnitDamagedPacket, &longer),
            Ok(s.len())
        );
        // One byte short.
        assert_eq!(
            batch_entry_len(sc::SCUnitDamagedPacket, &s.as_bytes()[..s.len() - 1]),
            Err(SizeError::Truncated)
        );

        // SCUnitHealedPacket with a plot cast action and an item caster.
        let mut s = PacketStream::new();
        s.write_u8(1)
            .write_u32(1)
            .write_u16(2)
            .write_u32(3)
            .write_u32(4);
        s.write_u8(2)
            .write_bc(5)
            .write_u64(6)
            .write_u32(7)
            .write_u8(0)
            .write_u32(0);
        s.write_bc(8)
            .write_u8(0)
            .write_u8(1)
            .write_i32(500)
            .write_i32(0)
            .write_u8(1);
        assert_eq!(
            batch_entry_len(sc::SCUnitHealedPacket, s.as_bytes()),
            Ok(s.len())
        );

        // SCPlotEventPacket: two units, 2 target units, flag 8 adds 52 bytes.
        for (flag, extra) in [(2u8, 0usize), (8, 52)] {
            let mut s = PacketStream::new();
            s.write_u16(12).write_u32(100).write_u32(200);
            s.write_u8(1).write_bc(1).write_u8(1).write_bc(2);
            s.write_u64(0)
                .write_bc(0)
                .write_u16(0)
                .write_bc(0)
                .write_i16(0);
            s.write_u8(2).write_bc(2).write_bc(2).write_u8(flag);
            s.write_raw(&vec![0; extra]);
            assert_eq!(
                batch_entry_len(sc::SCPlotEventPacket, s.as_bytes()),
                Ok(s.len())
            );
        }
        // A position plot object is 1 + 12 bytes.
        let mut s = PacketStream::new();
        s.write_u16(1).write_u32(1).write_u32(1);
        s.write_u8(2)
            .write_position(1.0, 2.0, 3.0)
            .write_raw(&[0, 0, 0]);
        s.write_u8(1).write_bc(2);
        s.write_u64(0)
            .write_bc(0)
            .write_u16(0)
            .write_bc(0)
            .write_i16(0);
        s.write_u8(0).write_u8(0);
        assert_eq!(
            batch_entry_len(sc::SCPlotEventPacket, s.as_bytes()),
            Ok(s.len())
        );

        // SCSkillCooldownResetPacket.
        let mut s = PacketStream::new();
        s.write_bc(1).write_u32(2).write_u32(0).write_bool(false);
        assert_eq!(
            batch_entry_len(sc::SCSkillCooldownResetPacket, s.as_bytes()),
            Ok(12)
        );
    }

    #[test]
    fn sizes_the_skill_lifecycle_packets() {
        // SCSkillStartedPacket: unit caster, position target, text skill object, extra flags 1|4.
        let mut s = PacketStream::new();
        s.write_u32(9).write_u16(1);
        s.write_u8(0).write_bc(1);
        s.write_u8(1)
            .write_i64(1)
            .write_i64(2)
            .write_f32(3.0)
            .write_f32(0.0)
            .write_bc(0)
            .write_bc(0);
        s.write_u8(3).write_str("hi");
        s.write_u16(10)
            .write_u16(10)
            .write_u8(0)
            .write_u8(5)
            .write_u8(2)
            .write_u32(77);
        assert_eq!(
            batch_entry_len(sc::SCSkillStartedPacket, s.as_bytes()),
            Ok(s.len())
        );
        // SCSkillFiredPacket: item caster, doodad target, no skill object.
        let mut s = PacketStream::new();
        s.write_u32(9).write_u16(1);
        s.write_u8(2)
            .write_bc(1)
            .write_u64(2)
            .write_u32(3)
            .write_u8(0)
            .write_u32(0);
        s.write_u8(4).write_bc(5);
        s.write_u8(0);
        s.write_i16(1)
            .write_i16(2)
            .write_u8(0)
            .write_u32(0)
            .write_u8(0);
        assert_eq!(
            batch_entry_len(sc::SCSkillFiredPacket, s.as_bytes()),
            Ok(s.len())
        );
        // SCBuffCreatedPacket: mount caster, 4-value pisc.
        let mut s = PacketStream::new();
        s.write_u8(3).write_bc(1).write_u32(2);
        s.write_u32(1)
            .write_bc(2)
            .write_u32(3)
            .write_u32(4)
            .write_u8(1)
            .write_i16(1)
            .write_u32(0);
        s.write_pisc(&[1, 600, 0, 0]);
        assert_eq!(
            batch_entry_len(sc::SCBuffCreatedPacket, s.as_bytes()),
            Ok(s.len())
        );
        // Fixed-size ones.
        let zeros = [0u8; 16];
        for (op, len) in [
            (sc::SCSkillEndedPacket, 2),
            (sc::SCPlotEndedPacket, 2),
            (sc::SCCastingStoppedPacket, 6),
            (sc::SCBuffRemovedPacket, 7),
            (sc::SCBuffLearnedPacket, 7),
            (sc::SCSkillStoppedPacket, 7),
            (sc::SCErrorMsgPacket, 9),
        ] {
            assert_eq!(batch_entry_len(op, &zeros), Ok(len), "opcode {op:#x}");
        }
    }

    #[test]
    fn rejects_unknown_opcodes_and_bad_enums() {
        assert_eq!(
            batch_entry_len(sc::SCUnitDeathPacket, &[0; 64]),
            Err(SizeError::Unsupported)
        );
        // Unknown cast action type 9.
        assert_eq!(
            batch_entry_len(sc::SCUnitHealedPacket, &[9; 64]),
            Err(SizeError::Invalid)
        );
        // A claimed target-unit count that runs past the entry.
        let mut s = PacketStream::new();
        s.write_u16(1).write_u32(1).write_u32(1);
        s.write_u8(1).write_bc(1).write_u8(1).write_bc(2);
        s.write_u64(0)
            .write_bc(0)
            .write_u16(0)
            .write_bc(0)
            .write_i16(0);
        s.write_u8(200);
        assert_eq!(
            batch_entry_len(sc::SCPlotEventPacket, s.as_bytes()),
            Err(SizeError::Truncated)
        );
    }

    #[test]
    fn split_batch_keeps_the_prefix_before_an_unsized_entry() {
        let mut raw = PacketStream::new();
        raw.write_u16(0)
            .write_u16(sc::SCSkillEndedPacket)
            .write_u16(0x0102);
        raw.write_u16(0)
            .write_u16(sc::SCUnitDeathPacket)
            .write_raw(&[1, 2, 3]);
        raw.write_u16(0)
            .write_u16(sc::SCSkillEndedPacket)
            .write_u16(0x0304);
        let raw = Bytes::from(raw.into_bytes());
        let mut out = VecDeque::new();
        assert_eq!(
            split_batch(&raw, 3, &mut out),
            Err(BatchError::Unsupported(sc::SCUnitDeathPacket))
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].opcode, sc::SCSkillEndedPacket);
        assert_eq!(&out[0].body[..], &[2, 1]);
        // The body is a view of the batch, not a copy.
        assert_eq!(out[0].body.as_ptr(), raw[4..].as_ptr());
    }

    #[test]
    fn split_batch_rejects_nonzero_headers_and_oversized_counts() {
        let raw = Bytes::from_static(&[1, 0, 0x10, 0, 0, 0]);
        let mut out = VecDeque::new();
        assert_eq!(split_batch(&raw, 1, &mut out), Err(BatchError::Structure));
        assert_eq!(
            split_batch(&raw, (MAX_BATCH_ENTRIES + 1) as u16, &mut out),
            Err(BatchError::TooManyEntries)
        );
        assert!(out.is_empty());
    }

    #[test]
    fn inflate_is_capped_without_preallocating_the_cap() {
        // A stored block round-trips.
        let payload: Vec<u8> = (0..2000u32).map(|i| i as u8).collect();
        let stored = miniz_oxide::deflate::compress_to_vec(&payload, 6);
        assert_eq!(inflate_batch(&stored).unwrap(), payload);
        // A bomb: 4 MiB of zeros deflates to a few KiB but must be refused at the cap.
        let bomb = miniz_oxide::deflate::compress_to_vec(&vec![0u8; 4 * 1024 * 1024], 6);
        assert!(bomb.len() < MAX_FRAME);
        assert_eq!(inflate_batch(&bomb), Err(BatchError::TooLarge));
        // Garbage.
        assert_eq!(inflate_batch(&[0xFF; 32]), Err(BatchError::Inflate));
    }
}
