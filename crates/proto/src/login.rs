//! Login server client (C2L / L2C) for the 10.0.2.13 client and the AAEmu 10.x login server.
//!
//! Frames are `[u16 len][u16 opcode][body]`, plaintext. A bot logs in with
//! `CARequestWebAuthPacket` (the launcher's JSON blob with the password) and answers an
//! `ACChallenge2Packet` (V2 challenge) when the server sends one instead. A login server with
//! `AutoAccount` creates unknown accounts on first login.
//!
//! A bot uses the login server exactly once per session: [`LoginClient::auth`], then
//! [`LoginClient::list_worlds`] and [`LoginClient::enter_world`]; the returned [`WorldCookie`]
//! is what the game server wants in `X2EnterWorldPacket`.

use std::io;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::Duration;

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_util::codec::Framed;

use crate::codec::{FrameCodec, Parts};
use crate::crypto::challenge;
use crate::stream::{MarshalError, PacketStream};

/// C2L opcodes (`AAEmu.Login` `CLOffsets`).
pub mod ca {
    /// `CARequestAuthPacket` (V2 challenge flow).
    pub const REQUEST_AUTH: u16 = 0x001;
    /// `CARequestWebAuthPacket` (launcher JSON).
    pub const REQUEST_WEB_AUTH: u16 = 0x002;
    /// `CAChallengeResponse2Packet`.
    pub const CHALLENGE_RESPONSE2: u16 = 0x004;
    /// `CAListWorldPacket`.
    pub const LIST_WORLD: u16 = 0x008;
    /// `CAEnterWorldPacket`.
    pub const ENTER_WORLD: u16 = 0x009;
    /// `CAPongPacket`.
    pub const PONG: u16 = 0x014;
}

/// L2C opcodes (`AAEmu.Login` `LCOffsets`).
pub mod ac {
    /// `ACJoinResponsePacket`.
    pub const JOIN_RESPONSE: u16 = 0x000;
    /// `ACAuthResponsePacket`.
    pub const AUTH_RESPONSE: u16 = 0x003;
    /// `ACChallenge2Packet`.
    pub const CHALLENGE2: u16 = 0x004;
    /// `ACWorldListPacket`.
    pub const WORLD_LIST: u16 = 0x008;
    /// `ACWorldQueuePacket`.
    pub const WORLD_QUEUE: u16 = 0x009;
    /// `ACWorldCookiePacket`.
    pub const WORLD_COOKIE: u16 = 0x00a;
    /// `ACEnterWorldDeniedPacket`.
    pub const ENTER_WORLD_DENIED: u16 = 0x00b;
    /// `ACLoginDeniedPacket`.
    pub const LOGIN_DENIED: u16 = 0x00c;
    /// `ACPingPacket`.
    pub const PING: u16 = 0x010;
}

/// C# name of an L2C opcode, for logging.
pub fn ac_name(op: u16) -> &'static str {
    match op {
        ac::JOIN_RESPONSE => "ACJoinResponsePacket",
        ac::AUTH_RESPONSE => "ACAuthResponsePacket",
        ac::CHALLENGE2 => "ACChallenge2Packet",
        ac::WORLD_LIST => "ACWorldListPacket",
        ac::WORLD_QUEUE => "ACWorldQueuePacket",
        ac::WORLD_COOKIE => "ACWorldCookiePacket",
        ac::ENTER_WORLD_DENIED => "ACEnterWorldDeniedPacket",
        ac::LOGIN_DENIED => "ACLoginDeniedPacket",
        ac::PING => "ACPingPacket",
        _ => "ACUnknown",
    }
}

/// C# name of a C2L opcode, for logging.
pub fn ca_name(op: u16) -> &'static str {
    match op {
        ca::REQUEST_AUTH => "CARequestAuthPacket",
        ca::REQUEST_WEB_AUTH => "CARequestWebAuthPacket",
        ca::CHALLENGE_RESPONSE2 => "CAChallengeResponse2Packet",
        ca::LIST_WORLD => "CAListWorldPacket",
        ca::ENTER_WORLD => "CAEnterWorldPacket",
        ca::PONG => "CAPongPacket",
        _ => "CAUnknown",
    }
}

/// Why a login-server exchange failed.
#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    /// The TCP connection could not be opened.
    #[error("connecting to login server {addr}: {source}")]
    Connect {
        /// The address that was dialled.
        addr: String,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The socket failed after it was open.
    #[error("login socket error: {0}")]
    Io(#[from] io::Error),
    /// No packet arrived within [`LoginClient::timeout`].
    #[error("timed out waiting for a login packet")]
    Timeout,
    /// The server closed the connection.
    #[error("login server closed the connection")]
    Closed,
    /// `ACLoginDeniedPacket`: bad credentials, banned account, ... (the `LoginDeniedReason`).
    #[error("login denied (reason {0})")]
    Denied(u8),
    /// `ACJoinResponsePacket` with a non-zero reason.
    #[error("join refused (reason {0})")]
    JoinRefused(u16),
    /// `ACEnterWorldDeniedPacket`: the game server is down or refused the account.
    #[error("enter world denied (reason {0})")]
    EnterWorldDenied(u8),
    /// A packet other than the one the exchange expects.
    #[error("expected {expected} but got {got} (0x{opcode:03x})")]
    Unexpected {
        /// The packet that was expected.
        expected: &'static str,
        /// The name of the packet that arrived instead.
        got: &'static str,
        /// The opcode that arrived instead.
        opcode: u16,
    },
    /// A frame too short to hold an opcode.
    #[error("login frame too short")]
    ShortFrame,
    /// A packet body that does not match its layout.
    #[error("malformed login packet: {0}")]
    Malformed(#[from] MarshalError),
}

/// Result of a login-server exchange.
pub type Result<T> = std::result::Result<T, LoginError>;

/// A world of `ACWorldListPacket`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct World {
    /// Game server id, what [`LoginClient::enter_world`] takes.
    pub id: u8,
    /// Display name.
    pub name: String,
    /// Whether the game server is registered and accepting players.
    pub active: bool,
}

/// A character of `ACWorldListPacket`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginCharacter {
    /// Owning account.
    pub account_id: u32,
    /// Game server the character lives on.
    pub gs_id: u8,
    /// Character id.
    pub id: u32,
    /// Character name.
    pub name: String,
}

/// `ACWorldListPacket`.
#[derive(Clone, Debug, Default)]
pub struct WorldList {
    /// Game servers known to the login server.
    pub worlds: Vec<World>,
    /// The account's characters on all of them.
    pub characters: Vec<LoginCharacter>,
}

/// `ACWorldCookiePacket`: the one-time ticket for the game server and where to present it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorldCookie {
    /// The login connection id; `X2EnterWorldPacket.cookie`.
    pub cookie: u32,
    /// The game server endpoint; `0.0.0.0` means "the login server's host" (see
    /// [`WorldCookie::resolve`]).
    pub game_addr: SocketAddrV4,
}

impl WorldCookie {
    /// The game server endpoint, with an unspecified address replaced by `login_host`.
    pub fn resolve(&self, login_host: Ipv4Addr) -> SocketAddrV4 {
        if self.game_addr.ip().is_unspecified() {
            SocketAddrV4::new(login_host, self.game_addr.port())
        } else {
            self.game_addr
        }
    }
}

/// The launcher's web-auth JSON for `user` / `password` (JSON-escaped).
pub fn web_auth_json(user: &str, password: &str) -> String {
    fn esc(out: &mut String, v: &str) {
        for c in v.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
    }
    let mut s = String::from("{\"source\":\"playerbots\",\"StrUserName\":\"");
    esc(&mut s, user);
    s.push_str("\",\"strUserToken\":\"");
    esc(&mut s, password);
    s.push_str("\"}");
    s
}

/// One login-server connection.
pub struct LoginClient {
    framed: Framed<TcpStream, FrameCodec>,
    /// How long to wait for each server packet (default 15 s).
    pub timeout: Duration,
    /// The account id from `ACAuthResponsePacket`; 0 before [`LoginClient::auth`].
    pub account_id: u32,
    password: String,
}

impl LoginClient {
    /// Opens a TCP connection to the login server at `addr` (`host:port`).
    pub async fn connect(addr: &str) -> Result<Self> {
        let sock = TcpStream::connect(addr)
            .await
            .map_err(|source| LoginError::Connect {
                addr: addr.to_owned(),
                source,
            })?;
        sock.set_nodelay(true)?;
        Ok(Self {
            framed: Framed::with_capacity(sock, FrameCodec, 1024),
            timeout: Duration::from_secs(15),
            account_id: 0,
            password: String::new(),
        })
    }

    async fn send(&mut self, opcode: u16, body: &[u8]) -> Result<()> {
        let head = opcode.to_le_bytes();
        self.framed.send(Parts { head: &head, body }).await?;
        Ok(())
    }

    /// Next L2C packet as `(opcode, body)`; answers `ACPingPacket` on the way.
    async fn recv(&mut self) -> Result<(u16, Bytes)> {
        loop {
            let frame = tokio::time::timeout(self.timeout, self.framed.next())
                .await
                .map_err(|_| LoginError::Timeout)?
                .ok_or(LoginError::Closed)??;
            if frame.len() < 2 {
                return Err(LoginError::ShortFrame);
            }
            let op = u16::from_le_bytes([frame[0], frame[1]]);
            if op == ac::PING && frame.len() >= 10 {
                let body = frame.slice(2..10);
                self.send(ca::PONG, &body).await?;
                continue;
            }
            return Ok((op, frame.slice(2..)));
        }
    }

    /// `CARequestWebAuthPacket` (answering an `ACChallenge2Packet` if one comes), then
    /// `ACJoinResponsePacket` + `ACAuthResponsePacket`. Returns the account id.
    pub async fn auth(&mut self, user: &str, password: &str) -> Result<u32> {
        self.password = password.to_owned();
        let mut s = PacketStream::new();
        s.write_u32(0) // pFrom
            .write_u32(0) // pTo
            .write_u8(0) // svc
            .write_bool(false) // dev
            .write_str(&web_auth_json(user, password))
            .write_raw(&[0; 16]) // mac, mac2
            .write_u64(0) // cpu
            .write_bool(true) // is64Bit
            .write_bool(false) // isMultiClient
            .write_u8(0); // clientSerial
        self.send(ca::REQUEST_WEB_AUTH, s.as_bytes()).await?;
        loop {
            let (op, body) = self.recv().await?;
            let mut r = PacketStream::from_bytes(body.to_vec());
            match op {
                ac::JOIN_RESPONSE => {
                    r.read_u8()?; // auth id
                    let reason = r.read_u16()?;
                    if reason != 0 {
                        return Err(LoginError::JoinRefused(reason));
                    }
                }
                ac::CHALLENGE2 => {
                    let rounds = r.read_i32()? as u32;
                    let salt = String::from_utf8_lossy(&r.read_bytes_sized()?).into_owned();
                    let mut hc = [0u32; 8];
                    for v in &mut hc {
                        *v = r.read_u32()?;
                    }
                    let ch = challenge::response_v2(&self.password, &salt, rounds, &hc);
                    let mut w = PacketStream::new();
                    for v in ch {
                        w.write_u32(v);
                    }
                    self.send(ca::CHALLENGE_RESPONSE2, w.as_bytes()).await?;
                }
                ac::AUTH_RESPONSE => {
                    self.account_id = r.read_u64()? as u32;
                    return Ok(self.account_id);
                }
                ac::LOGIN_DENIED => return Err(LoginError::Denied(r.read_u8().unwrap_or(0))),
                _ => return Err(unexpected(ac::AUTH_RESPONSE, op)),
            }
        }
    }

    /// `CAListWorldPacket` then `ACWorldListPacket`.
    pub async fn list_worlds(&mut self) -> Result<WorldList> {
        self.send(ca::LIST_WORLD, &0u64.to_le_bytes()).await?;
        let (op, body) = self.recv().await?;
        if op == ac::LOGIN_DENIED {
            return Err(LoginError::Denied(body.first().copied().unwrap_or(0)));
        }
        if op != ac::WORLD_LIST {
            return Err(unexpected(ac::WORLD_LIST, op));
        }
        parse_world_list(&mut PacketStream::from_bytes(body.to_vec()))
    }

    /// `CAEnterWorldPacket` then `ACWorldCookiePacket` (an `ACWorldQueuePacket` before it is
    /// skipped).
    ///
    /// The server answers after the game server has accepted the account, which can take a
    /// moment; [`LoginClient::timeout`] applies. IPv6 game server hosts are not supported.
    pub async fn enter_world(&mut self, gs_id: u8) -> Result<WorldCookie> {
        let mut s = PacketStream::new();
        s.write_u64(0).write_u8(gs_id);
        self.send(ca::ENTER_WORLD, s.as_bytes()).await?;
        loop {
            let (op, body) = self.recv().await?;
            let mut r = PacketStream::from_bytes(body.to_vec());
            return match op {
                ac::WORLD_QUEUE => continue,
                ac::ENTER_WORLD_DENIED => {
                    Err(LoginError::EnterWorldDenied(r.read_u8().unwrap_or(0)))
                }
                ac::LOGIN_DENIED => Err(LoginError::Denied(r.read_u8().unwrap_or(0))),
                ac::WORLD_COOKIE => parse_world_cookie(&mut r),
                _ => Err(unexpected(ac::WORLD_COOKIE, op)),
            };
        }
    }
}

fn unexpected(want: u16, got: u16) -> LoginError {
    LoginError::Unexpected {
        expected: ac_name(want),
        got: ac_name(got),
        opcode: got,
    }
}

/// `ACWorldListPacket` (10.x): privacy flag, worlds, then the account's characters.
fn parse_world_list(r: &mut PacketStream) -> Result<WorldList> {
    let mut out = WorldList::default();
    r.read_bool()?; // privacy policy
    let n = r.read_u8()?;
    for _ in 0..n {
        let id = r.read_u8()?;
        r.read_u8()?; // parent
        r.read_u16()?; // type
        r.read_u8()?; // color
        let name = r.read_string()?;
        r.read_u8()?; // entry
        let active = r.read_u8()? != 0;
        if active {
            r.read_bytes(11)?; // load + 10 race congestion values
        }
        out.worlds.push(World { id, name, active });
    }
    let n = r.read_u8()?;
    for _ in 0..n {
        let account_id = r.read_u64()? as u32;
        let gs_id = r.read_u8()?;
        let id = r.read_u32()?;
        let name = r.read_string()?;
        r.read_bytes(2)?; // race, gender
        r.read_bytes_sized()?; // guid
        r.read_u64()?; // v
        out.characters.push(LoginCharacter {
            account_id,
            gs_id,
            id,
            name,
        });
    }
    Ok(out)
}

/// `ACWorldCookiePacket` body: `u32 cookie`, then four `(IPv4 octets reversed, u16 port)`
/// pairs; the first one is used.
fn parse_world_cookie(r: &mut PacketStream) -> Result<WorldCookie> {
    let cookie = r.read_u32()?;
    let ip = [r.read_u8()?, r.read_u8()?, r.read_u8()?, r.read_u8()?];
    let port = r.read_u16()?;
    Ok(WorldCookie {
        cookie,
        game_addr: SocketAddrV4::new(Ipv4Addr::new(ip[3], ip[2], ip[1], ip[0]), port),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_auth_json_escapes() {
        assert_eq!(
            web_auth_json("bo\"b", "p\\w"),
            r#"{"source":"playerbots","StrUserName":"bo\"b","strUserToken":"p\\w"}"#
        );
    }

    #[test]
    fn parses_the_10x_world_list() {
        let mut w = PacketStream::new();
        w.write_bool(false).write_u8(1);
        w.write_u8(1)
            .write_u8(0)
            .write_u16(0)
            .write_u8(0)
            .write_str("AAEmu")
            .write_u8(1);
        w.write_u8(1).write_u8(2).write_raw(&[0; 10]);
        w.write_u8(1)
            .write_u64(42)
            .write_u8(1)
            .write_u32(7)
            .write_str("Anna");
        w.write_u8(1)
            .write_u8(2)
            .write_bytes_sized(&[0; 16])
            .write_u64(0);
        let wl = parse_world_list(&mut PacketStream::from_bytes(w.as_bytes().to_vec())).unwrap();
        assert_eq!(
            wl.worlds,
            vec![World {
                id: 1,
                name: "AAEmu".into(),
                active: true
            }]
        );
        assert_eq!(wl.characters[0].name, "Anna");
        assert_eq!(wl.characters[0].account_id, 42);
    }

    #[test]
    fn cookie_address_is_unreversed() {
        let mut w = PacketStream::new();
        w.write_u32(99);
        for _ in 0..4 {
            w.write_raw(&[1, 0, 0, 127]).write_u16(1239);
        }
        let c = parse_world_cookie(&mut PacketStream::from_bytes(w.as_bytes().to_vec())).unwrap();
        assert_eq!(c.cookie, 99);
        assert_eq!(c.game_addr, "127.0.0.1:1239".parse().unwrap());
    }
}
