//! Guilds (the server calls them expeditions): the `CSCreateExpedition` .. `CSKickFromExpedition`
//! requests a bot sends and the `SCFactionCreated`, `SCExpedition*` and `SCUnitExpeditionChanged`
//! packets it reads.
//!
//! All of them are level-1 (`LEVEL_PLAIN`) packets. Paths in the doc comments are relative to
//! `AAEmu-1.2-Rust/rust/crates/aaemu-game/src/` (`X/` = `systems/social/`).
//!
//! ```text
//! faction  = u32 id, bool aggro_link, u32 mother, str name, u32 owner, str owner_name,
//!            i8 owner_type, u8 political_system, i64 created, bool diplomacy_target,
//!            u8 allow_change_name                     (packets/world.rs:355, `write_system_faction`)
//! member   = u32 guild, u32 character, bool in_party, bool online, i64 last_leave, str name,
//!            u8 level, u32 zone, u32 id3, u8 ability x3, u8 role, i64 x, i64 y, f32 z, str memo,
//!            i64 transfer_requested                   (X/expedition.rs:48, `ExpeditionMember`)
//! ```
//!
//! Server rules the bots rely on (all in `X/expedition.rs`): the founder must be in a team of
//! `Create.PartyMemberCount` (5) members that are all level 5 or more, unguilded and of the
//! founder's mother faction, and pays `Create.Cost` (10000 copper); the name matches
//! `^[a-zA-Z ]{3,32}$` (letters of the Cyrillic alphabet also pass) and no guild has it; every
//! member of the team joins at once (role 0). Only the roles in [`role`] with the `invite`
//! policy (officer, commander, general) can invite; only the general promotes. Invitations
//! expire after 60 s, an unanswered one is replaced by the next, and a refused request is
//! silent (no answer at all), except the founding checks, which answer `SCErrorMsg` ([`err`]).
//!
//! [`parse`] turns the packets into a [`GuildEvent`]; the request builders return an [`Out`].

use crate::game::LEVEL_GAME;
use crate::opcodes::{cs, sc};
use crate::stream::{MarshalError, PacketStream};

/// The roles of a guild member (`Expedition.json`, `RolePolicies`).
pub mod role {
    /// Apprentice: the role a new member starts with.
    pub const APPRENTICE: u8 = 0;
    /// Member.
    pub const MEMBER: u8 = 1;
    /// Officer: may invite.
    pub const OFFICER: u8 = 2;
    /// Commander: may invite and expel.
    pub const COMMANDER: u8 = 3;
    /// General: the owner of the guild.
    pub const OWNER: u8 = 255;

    /// Whether the default policy lets `role` invite (officer and above).
    pub fn can_invite(role: u8) -> bool {
        role >= OFFICER
    }

    /// The name of `role` (the default policy name).
    pub fn name(role: u8) -> &'static str {
        match role {
            APPRENTICE => "Apprentice",
            MEMBER => "Member",
            OFFICER => "Officer",
            COMMANDER => "Commander",
            OWNER => "General",
            _ => "Member",
        }
    }
}

/// `ErrorMessageType` ids of the founding checks (`X/util.rs:12-34`), the answers to a refused
/// [`create`].
pub mod err {
    /// A member of the team is below the founding level.
    pub const CREATE_LEVEL: i16 = 293;
    /// The founder is not in a team, or the team is too small.
    pub const CREATE_MEMBER: i16 = 294;
    /// The founder cannot pay the founding cost.
    pub const CREATE_MONEY: i16 = 295;
    /// The name has characters the name pattern does not allow.
    pub const NAME_CHARACTER: i16 = 296;
    /// The name is longer than 32 characters.
    pub const NAME_LENGTH: i16 = 297;
    /// A guild of that name exists.
    pub const NAME_EXISTS: i16 = 299;
    /// A member of the team is of another mother faction.
    pub const CREATE_FACTION: i16 = 321;
    /// A member of the team is in a guild already.
    pub const CREATE_MEMBER_GUILDED: i16 = 330;
    /// The founder is in a guild already.
    pub const ALREADY_MEMBER: i16 = 761;
    /// A guild chat line from a character that is in no guild.
    pub const CHAT_NOT_IN_GUILD: i16 = 311;
    /// Only the owner may do this.
    pub const ONLY_OWNER: i16 = 807;
    /// Only a member may do this.
    pub const ONLY_MEMBER: i16 = 808;

    /// Whether `code` is one of the founding refusals.
    pub fn is_founding(code: i16) -> bool {
        matches!(
            code,
            CREATE_LEVEL
                | CREATE_MEMBER
                | CREATE_MONEY
                | NAME_CHARACTER
                | NAME_LENGTH
                | NAME_EXISTS
                | CREATE_FACTION
                | CREATE_MEMBER_GUILDED
                | ALREADY_MEMBER
        )
    }
}

/// The founding cost in copper (`Expedition.json`, `Create.Cost`).
pub const CREATE_COST: i64 = 10_000;
/// The level every member of the founding team needs (`Create.Level`).
pub const CREATE_LEVEL: u8 = 5;
/// The size of the founding team (`Create.PartyMemberCount`).
pub const CREATE_TEAM: usize = 5;
/// Seconds an invitation can be answered (`INVITATION_TIMEOUT_MS`, `X/expedition.rs:198`).
pub const INVITE_SECS: u64 = 60;

// ---- requests -------------------------------------------------------------------------------

fn request(opcode: u16, fill: impl FnOnce(&mut PacketStream)) -> crate::cs::Out {
    let mut s = PacketStream::new();
    fill(&mut s);
    crate::cs::Out {
        level: LEVEL_GAME,
        opcode,
        body: s.into_bytes(),
    }
}

/// `CSCreateExpeditionPacket`: founds a guild with the members of the sender's team. Server
/// read: `X/packets/expedition.rs:125`. Body: `str name, u32 id` (the client sends 0).
pub fn create(name: &str, id: u32) -> crate::cs::Out {
    request(cs::CSCreateExpeditionPacket, |s| {
        s.write_str(name).write_u32(id);
    })
}

/// `CSInviteToExpeditionPacket`: invites the character `name` (any distance). Server read:
/// `X/packets/expedition.rs:195`. Body: `str name`.
pub fn invite(name: &str) -> crate::cs::Out {
    request(cs::CSInviteToExpeditionPacket, |s| {
        s.write_str(name);
    })
}

/// `CSReplyExpeditionInvitationPacket`: answers the invitation of [`GuildEvent::Invitation`].
/// Server read: `X/packets/expedition.rs:207`. Body: `u32 guild, u32 inviter character, bool
/// join`. The server accepts only the guild it invited to (`faction`) and consumes the
/// invitation with any answer.
pub fn reply_invite(faction: u32, inviter: u32, join: bool) -> crate::cs::Out {
    request(cs::CSReplyExpeditionInvitationPacket, |s| {
        s.write_u32(faction).write_u32(inviter).write_bool(join);
    })
}

/// `CSLeaveExpeditionPacket`: leaves the guild. An owner who leaves leaves the guild without a
/// leader (`X/expedition.rs:927`): hand the guild over first ([`change_owner`]). Empty body.
pub fn leave() -> crate::cs::Out {
    request(cs::CSLeaveExpeditionPacket, |_| {})
}

/// `CSKickFromExpeditionPacket`: expels a member of a lower rank (commander or general only).
/// Server read: `X/packets/expedition.rs:231`. Body: `u32 character`.
pub fn kick(character: u32) -> crate::cs::Out {
    request(cs::CSKickFromExpeditionPacket, |s| {
        s.write_u32(character);
    })
}

/// `CSChangeExpeditionMemberRolePacket`: gives a member a role below the sender's (general
/// only, by the default policy). Server read: `X/packets/expedition.rs:149`. Body: `u8 role,
/// u32 character`.
pub fn set_member_role(role: u8, character: u32) -> crate::cs::Out {
    request(cs::CSChangeExpeditionMemberRolePacket, |s| {
        s.write_u8(role).write_u32(character);
    })
}

/// `CSChangeExpeditionOwnerPacket`: hands the guild to a member (the sender becomes an
/// apprentice). Server read: `X/packets/expedition.rs:161`. Body: `u32 character`.
pub fn change_owner(character: u32) -> crate::cs::Out {
    request(cs::CSChangeExpeditionOwnerPacket, |s| {
        s.write_u32(character);
    })
}

/// `CSDismissExpeditionPacket`: disbands the guild (general only). Empty body.
pub fn dismiss() -> crate::cs::Out {
    request(cs::CSDismissExpeditionPacket, |_| {})
}

// ---- events ---------------------------------------------------------------------------------

/// A guild as `write_system_faction` writes it (`packets/world.rs:355`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactionBrief {
    /// Guild id.
    pub id: u32,
    /// The server stores the founder's faction here.
    pub mother_id: u32,
    /// Guild name.
    pub name: Box<str>,
    /// Character id of the owner.
    pub owner_id: u32,
    /// Name of the owner.
    pub owner_name: Box<str>,
    /// Unix seconds the guild was founded.
    pub created: i64,
}

/// A member as `ExpeditionMember.Write` writes it (`X/expedition.rs:48`), reduced to what a bot
/// uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberBrief {
    /// Guild id.
    pub guild: u32,
    /// Character id.
    pub char_id: u32,
    /// The member is online.
    pub online: bool,
    /// Name.
    pub name: Box<str>,
    /// Level.
    pub level: u8,
    /// Role, see [`role`].
    pub role: u8,
}

/// A packet about guilds. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuildEvent {
    /// `SCFactionList` (0x008): guilds that exist, `u8 n` and `n` factions, at most 20 a packet
    /// (`X/expedition.rs:491`). Sent at enter-world (all guilds, in chunks) and to everybody
    /// when a guild is founded (that one).
    Factions(Box<[FactionBrief]>),
    /// `SCFactionCreated` (0x013): the guild the sender founded exists. `faction`, `bc owner
    /// object`, `u8 n`, `n x (bc object, u32 character, str name)` (`X/expedition.rs:708`,
    /// `X/packets/expedition.rs:73`). Sent to the founder only.
    Created {
        /// The new guild.
        faction: Box<FactionBrief>,
        /// Object id of the founder.
        owner_obj: u32,
    },
    /// `SCExpeditionMemberList` (0x016): a chunk of the member list (20 members at most),
    /// `u32 total, u8 n, u32 guild`, `n` members. Sent at enter-world, on joining and on founding.
    MemberList {
        /// Members in the whole list.
        total: u32,
        /// Guild id.
        guild: u32,
        /// The members of this chunk.
        members: Box<[MemberBrief]>,
    },
    /// `SCExpeditionMemberStatusChanged` (0x017): a member logged in or out, or joined. `member`,
    /// `u8 flag`.
    MemberStatus {
        /// The member as the server knows it now.
        member: Box<MemberBrief>,
        /// The packet's flag (0 on every send of the server).
        flag: u8,
    },
    /// `SCExpeditionInvitation` (0x01a): we are invited. `u32 inviter, str inviter name, u32
    /// guild, str guild name`.
    Invitation {
        /// Character id of the inviter.
        inviter: u32,
        /// Name of the inviter.
        inviter_name: Box<str>,
        /// Guild id (what [`reply_invite`] echoes).
        guild: u32,
        /// Guild name.
        guild_name: Box<str>,
    },
    /// `SCUnitExpeditionChanged` (0x01c): a character joined (`to` is the guild), left, or was
    /// expelled (`to` is 0). `bc object, u32 character, str kicker, str name, u32 from, u32 to,
    /// bool expel`. Sent to the character, to those who see it and, for a leave or an expulsion,
    /// to the guild.
    UnitChanged {
        /// Object id of the character (0 when it is offline).
        obj: u32,
        /// Character id.
        char_id: u32,
        /// Name of the member who expelled it, empty otherwise.
        kicker: Box<str>,
        /// Character name.
        name: Box<str>,
        /// The guild it left (0 when it joined).
        from: u32,
        /// The guild it joined (0 when it left).
        to: u32,
        /// It was expelled.
        expel: bool,
    },
    /// `SCExpeditionRoleChanged` (0x00d): `u32 character, u8 role, str name`.
    RoleChanged {
        /// Character id.
        char_id: u32,
        /// The new role.
        role: u8,
        /// Character name.
        name: Box<str>,
    },
    /// `SCExpeditionOwnerChanged` (0x00e): `u32 old owner, u32 new owner, str new owner name`.
    OwnerChanged {
        /// The former owner's character id.
        from: u32,
        /// The new owner's character id.
        to: u32,
        /// The new owner's name.
        name: Box<str>,
    },
    /// `SCExpeditionDismissed` (0x015): the guild is gone. `u32 guild, bool success`.
    Dismissed {
        /// Guild id.
        guild: u32,
        /// The server's flag (always true).
        success: bool,
    },
}

/// The smallest `ExpeditionMember.Write`: empty strings.
const MIN_MEMBER_LEN: usize = 4 + 4 + 1 + 1 + 8 + 2 + 1 + 4 + 4 + 3 + 1 + 8 + 8 + 4 + 2 + 8;

/// The smallest `write_system_faction`: empty strings.
const MIN_FACTION_LEN: usize = 4 + 1 + 4 + 2 + 4 + 2 + 1 + 1 + 8 + 1 + 1;

/// 10.x `ExpeditionMember.Write`: i32 guild, u64 character, in party, online, last leave time,
/// name, level, heir level, u32 zone, i32 faction, three abilities, role, fx64 x, fx64 y, f32 z,
/// memo, transfer time, contribution, weekly contribution, gear score.
fn read_member(s: &mut PacketStream) -> Result<MemberBrief, MarshalError> {
    let guild = s.read_u32()?;
    let char_id = s.read_u64()? as u32;
    s.read_bool()?; // in party
    let online = s.read_bool()?;
    s.read_i64()?; // last time in the world
    let name = s.read_string()?.into_boxed_str();
    let level = s.read_u8()?;
    s.read_u8()?; // heir level
    s.read_u32()?; // zone
    s.read_u32()?; // faction
    s.read_bytes(3)?; // abilities
    let role = s.read_u8()?;
    s.read_i64()?; // x
    s.read_i64()?; // y
    s.read_f32()?; // z
    s.read_string()?; // memo
    s.read_i64()?; // transfer requested
    s.read_bytes(4 + 4 + 4)?; // contribution, weekly contribution, gear score
    Ok(MemberBrief {
        guild,
        char_id,
        online,
        name,
        level,
        role,
    })
}

fn read_faction(s: &mut PacketStream) -> Result<FactionBrief, MarshalError> {
    let id = s.read_u32()?;
    s.read_bool()?; // aggro link
    let mother_id = s.read_u32()?;
    let name = s.read_string()?.into_boxed_str();
    let owner_id = s.read_u32()?;
    let owner_name = s.read_string()?.into_boxed_str();
    s.read_i8()?; // owner type
    s.read_u8()?; // political system
    let created = s.read_i64()?;
    s.read_bool()?; // diplomacy target
    s.read_u8()?; // allow change name
    Ok(FactionBrief {
        id,
        mother_id,
        name,
        owner_id,
        owner_name,
        created,
    })
}

type Reader = fn(&mut PacketStream) -> Result<GuildEvent, MarshalError>;

/// Parses one S→C game packet body (opcode already stripped). `Ok(None)` when the packet is not a
/// guild packet of a bot's interest or is on another channel level; `Err` when a packet of this
/// module does not match its layout.
pub fn parse(level: u8, opcode: u16, body: &[u8]) -> Result<Option<GuildEvent>, MarshalError> {
    if level != LEVEL_GAME {
        return Ok(None);
    }
    let read: Reader = match opcode {
        sc::SCFactionListPacket => |s| {
            let n = usize::from(s.read_u8()?);
            if n.saturating_mul(MIN_FACTION_LEN) > s.remaining() {
                return Err(MarshalError::EndOfStream {
                    pos: s.pos(),
                    wanted: n.saturating_mul(MIN_FACTION_LEN),
                    len: s.len(),
                });
            }
            let mut list = Vec::with_capacity(n);
            for _ in 0..n {
                list.push(read_faction(s)?);
            }
            Ok(GuildEvent::Factions(list.into_boxed_slice()))
        },
        sc::SCFactionCreatedPacket => |s| {
            let faction = Box::new(read_faction(s)?);
            Ok(GuildEvent::Created {
                faction,
                owner_obj: s.read_bc()?,
            })
        },
        sc::SCExpeditionMemberListPacket => |s| {
            let total = s.read_u32()?;
            let n = usize::from(s.read_u8()?);
            let guild = s.read_u32()?;
            if n.saturating_mul(MIN_MEMBER_LEN) > s.remaining() {
                return Err(MarshalError::EndOfStream {
                    pos: s.pos(),
                    wanted: n.saturating_mul(MIN_MEMBER_LEN),
                    len: s.len(),
                });
            }
            let mut members = Vec::with_capacity(n);
            for _ in 0..n {
                members.push(read_member(s)?);
            }
            Ok(GuildEvent::MemberList {
                total,
                guild,
                members: members.into_boxed_slice(),
            })
        },
        sc::SCExpeditionMemberStatusChangedPacket => |s| {
            let member = Box::new(read_member(s)?);
            Ok(GuildEvent::MemberStatus {
                member,
                flag: s.read_u8()?,
            })
        },
        sc::SCExpeditionInvitationPacket => |s| {
            Ok(GuildEvent::Invitation {
                inviter: s.read_u64()? as u32,
                inviter_name: s.read_string()?.into_boxed_str(),
                guild: s.read_u32()?,
                guild_name: s.read_string()?.into_boxed_str(),
            })
        },
        sc::SCUnitExpeditionChangedPacket => |s| {
            Ok(GuildEvent::UnitChanged {
                obj: s.read_bc()?,
                char_id: s.read_u32()?,
                kicker: s.read_string()?.into_boxed_str(),
                name: s.read_string()?.into_boxed_str(),
                from: s.read_u32()?,
                to: s.read_u32()?,
                expel: s.read_bool()?,
            })
        },
        sc::SCExpeditionRoleChangedPacket => |s| {
            // 10.x: u64 character, str name, u32 role
            let char_id = s.read_u64()? as u32;
            let name = s.read_string()?.into_boxed_str();
            let role = s.read_u32()? as u8;
            Ok(GuildEvent::RoleChanged {
                char_id,
                role,
                name,
            })
        },
        sc::SCExpeditionOwnerChangedPacket => |s| {
            Ok(GuildEvent::OwnerChanged {
                from: s.read_u64()? as u32,
                to: s.read_u64()? as u32,
                name: s.read_string()?.into_boxed_str(),
            })
        },
        sc::SCExpeditionDismissedPacket => |s| {
            Ok(GuildEvent::Dismissed {
                guild: s.read_u32()?,
                success: s.read_bool()?,
            })
        },
        _ => return Ok(None),
    };
    read(&mut PacketStream::from_bytes(body)).map(Some)
}

#[cfg(test)]
mod tests;
