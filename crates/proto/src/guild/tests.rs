//! Byte-exact tests of the guild packets: every expected body is written out by hand from the
//! server's writers and readers (`X/` = `aaemu-game/src/systems/social/`), not produced by the
//! builders under test.

use super::*;
use crate::opcodes::sc as op;
use crate::sc::ServerEvent;
use crate::sc_activity::ActivityEvent;

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// A `str`: `u16` length, then the bytes.
fn st(text: &str) -> Vec<u8> {
    let mut v = (text.len() as u16).to_le_bytes().to_vec();
    v.extend_from_slice(text.as_bytes());
    v
}

fn le32(v: u32) -> [u8; 4] {
    v.to_le_bytes()
}

fn le64(v: u64) -> [u8; 8] {
    v.to_le_bytes()
}

#[track_caller]
fn check(out: crate::cs::Out, opcode: u16, body: &[u8]) {
    assert_eq!(out.level, LEVEL_GAME);
    assert_eq!(out.opcode, opcode, "opcode of {}", out.name());
    assert_eq!(out.body, body, "body of {}", out.name());
}

/// `write_system_faction` (`packets/world.rs:355`).
fn faction(
    id: u32,
    mother: u32,
    name: &str,
    owner: u32,
    owner_name: &str,
    created: u64,
) -> Vec<u8> {
    cat(&[
        &le32(id),
        &[0], // aggro link
        &le32(mother),
        &st(name),
        &le32(owner),
        &st(owner_name),
        &[0],           // owner type
        &[1],           // political system
        &le64(created), // created
        &[0],           // diplomacy target
        &[0],           // allow change name
    ])
}

/// `ExpeditionMember.Write` (`X/expedition.rs:48`).
fn member(guild: u32, char_id: u32, online: bool, name: &str, level: u8, role: u8) -> Vec<u8> {
    cat(&[
        &le32(guild),
        &le32(char_id),
        &[0], // in party
        &[u8::from(online)],
        &le64(1_700_000_000), // last time in the world
        &st(name),
        &[level],
        &le32(7), // zone
        &le32(0), // id3
        &[1, 11, 11],
        &[role],
        &le64(0x1234_5678), // x
        &le64(0x9abc_def0), // y
        &1.5f32.to_le_bytes(),
        &st("memo"),
        &le64(0),
    ])
}

#[test]
#[ignore = "1.2 byte layout; the 10.x layouts are checked against captured packets (tests/capture_10x.rs)"]
fn the_requests_are_byte_exact() {
    // CSCreateExpeditionPacket: str name, u32 id
    check(
        create("Silent Wardens", 0),
        cs::CSCreateExpeditionPacket,
        &cat(&[&st("Silent Wardens"), &le32(0)]),
    );
    assert_eq!(cs::CSCreateExpeditionPacket, 0x004);
    // CSInviteToExpeditionPacket: str name
    check(invite("Anna"), cs::CSInviteToExpeditionPacket, &st("Anna"));
    assert_eq!(cs::CSInviteToExpeditionPacket, 0x00c);
    // CSReplyExpeditionInvitationPacket: u32 id, u32 id2, bool join
    check(
        reply_invite(9, 33, true),
        cs::CSReplyExpeditionInvitationPacket,
        &cat(&[&le32(9), &le32(33), &[1]]),
    );
    check(
        reply_invite(9, 33, false),
        cs::CSReplyExpeditionInvitationPacket,
        &cat(&[&le32(9), &le32(33), &[0]]),
    );
    assert_eq!(cs::CSReplyExpeditionInvitationPacket, 0x00d);
    // CSLeaveExpeditionPacket and CSDismissExpeditionPacket: empty
    check(leave(), cs::CSLeaveExpeditionPacket, &[]);
    assert_eq!(cs::CSLeaveExpeditionPacket, 0x00e);
    check(dismiss(), cs::CSDismissExpeditionPacket, &[]);
    assert_eq!(cs::CSDismissExpeditionPacket, 0x00b);
    // CSKickFromExpeditionPacket: u32 id
    check(kick(77), cs::CSKickFromExpeditionPacket, &le32(77));
    assert_eq!(cs::CSKickFromExpeditionPacket, 0x00f);
    // CSChangeExpeditionMemberRolePacket: u8 role, u32 id
    check(
        set_member_role(role::OFFICER, 41),
        cs::CSChangeExpeditionMemberRolePacket,
        &cat(&[&[2], &le32(41)]),
    );
    assert_eq!(cs::CSChangeExpeditionMemberRolePacket, 0x007);
    // CSChangeExpeditionOwnerPacket: u32 id
    check(
        change_owner(41),
        cs::CSChangeExpeditionOwnerPacket,
        &le32(41),
    );
    assert_eq!(cs::CSChangeExpeditionOwnerPacket, 0x008);
}

fn event(opcode: u16, body: &[u8]) -> GuildEvent {
    parse(LEVEL_GAME, opcode, body)
        .expect("parses")
        .expect("a guild packet")
}

#[test]
#[ignore = "1.2 byte layout; the 10.x layouts are checked against captured packets (tests/capture_10x.rs)"]
fn a_created_guild_is_read() {
    // SCFactionCreatedPacket: faction, bc owner obj, u8 n, n x (bc, u32, str)
    let mut body = faction(5, 101, "Silent Wardens", 33, "Anna", 1_700_000_001);
    body.extend_from_slice(&[0x03, 0x02, 0x01, 1, 0x03, 0x02, 0x01]);
    body.extend_from_slice(&le32(33));
    body.extend_from_slice(&st("Anna"));
    match event(op::SCFactionCreatedPacket, &body) {
        GuildEvent::Created { faction, owner_obj } => {
            assert_eq!(owner_obj, 0x010203);
            assert_eq!(faction.id, 5);
            assert_eq!(faction.mother_id, 101);
            assert_eq!(&*faction.name, "Silent Wardens");
            assert_eq!(faction.owner_id, 33);
            assert_eq!(&*faction.owner_name, "Anna");
            assert_eq!(faction.created, 1_700_000_001);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(op::SCFactionCreatedPacket, 0x013);
}

#[test]
#[ignore = "1.2 byte layout; the 10.x layouts are checked against captured packets (tests/capture_10x.rs)"]
fn the_list_of_guilds_is_read() {
    // SCFactionListPacket: u8 n, n x faction
    let mut body = vec![2u8];
    body.extend(faction(5, 101, "Silent Wardens", 33, "Anna", 1_700_000_001));
    body.extend(faction(6, 115, "Iron Pilgrims", 40, "Dov", 1_700_000_002));
    match event(op::SCFactionListPacket, &body) {
        GuildEvent::Factions(list) => {
            assert_eq!(list.len(), 2);
            assert_eq!((list[0].id, &*list[0].name), (5, "Silent Wardens"));
            assert_eq!(
                (list[1].id, list[1].mother_id, &*list[1].name),
                (6, 115, "Iron Pilgrims")
            );
        }
        other => panic!("{other:?}"),
    }
    // the empty list the server sends to a character that has none to show
    match event(op::SCFactionListPacket, &[0]) {
        GuildEvent::Factions(list) => assert!(list.is_empty()),
        other => panic!("{other:?}"),
    }
    // a count that does not fit the bytes is an error
    assert!(parse(LEVEL_GAME, op::SCFactionListPacket, &[200, 1, 2]).is_err());
    assert_eq!(op::SCFactionListPacket, 0x008);
}

#[test]
#[ignore = "1.2 byte layout; the 10.x layouts are checked against captured packets (tests/capture_10x.rs)"]
fn a_member_list_chunk_is_read() {
    // SCExpeditionMemberListPacket: u32 total, u8 n, u32 id, members
    let mut body = cat(&[&le32(21), &[2], &le32(5)]);
    body.extend(member(5, 33, true, "Anna", 30, role::OWNER));
    body.extend(member(5, 34, false, "Bo", 12, role::APPRENTICE));
    match event(op::SCExpeditionMemberListPacket, &body) {
        GuildEvent::MemberList {
            total,
            guild,
            members,
        } => {
            assert_eq!((total, guild), (21, 5));
            assert_eq!(members.len(), 2);
            assert_eq!(
                members[0],
                MemberBrief {
                    guild: 5,
                    char_id: 33,
                    online: true,
                    name: "Anna".into(),
                    level: 30,
                    role: 255,
                }
            );
            assert_eq!(
                (members[1].char_id, members[1].online, members[1].role),
                (34, false, 0)
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(op::SCExpeditionMemberListPacket, 0x016);
}

#[test]
#[ignore = "1.2 byte layout; the 10.x layouts are checked against captured packets (tests/capture_10x.rs)"]
fn a_member_status_is_read() {
    // SCExpeditionMemberStatusChangedPacket: member, u8 flag
    let mut body = member(5, 35, true, "Cy", 8, role::MEMBER);
    body.push(0);
    match event(op::SCExpeditionMemberStatusChangedPacket, &body) {
        GuildEvent::MemberStatus { member, flag } => {
            assert_eq!((member.char_id, &*member.name, member.level), (35, "Cy", 8));
            assert_eq!((member.role, member.online, flag), (1, true, 0));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(op::SCExpeditionMemberStatusChangedPacket, 0x017);
}

#[test]
#[ignore = "1.2 byte layout; the 10.x layouts are checked against captured packets (tests/capture_10x.rs)"]
fn an_invitation_is_read() {
    // SCExpeditionInvitationPacket: u32 invitor, str, u32 faction, str faction name
    let body = cat(&[&le32(33), &st("Anna"), &le32(5), &st("Silent Wardens")]);
    assert_eq!(
        event(op::SCExpeditionInvitationPacket, &body),
        GuildEvent::Invitation {
            inviter: 33,
            inviter_name: "Anna".into(),
            guild: 5,
            guild_name: "Silent Wardens".into(),
        }
    );
    assert_eq!(op::SCExpeditionInvitationPacket, 0x01a);
}

#[test]
#[ignore = "1.2 byte layout; the 10.x layouts are checked against captured packets (tests/capture_10x.rs)"]
fn a_join_a_leave_and_an_expulsion_are_read() {
    // SCUnitExpeditionChangedPacket: bc obj, u32 char, str kicker, str name, u32 id, u32 to, bool
    let join = cat(&[
        &[0x03, 0x02, 0x01],
        &le32(34),
        &st(""),
        &st("Bo"),
        &le32(0),
        &le32(5),
        &[0],
    ]);
    assert_eq!(
        event(op::SCUnitExpeditionChangedPacket, &join),
        GuildEvent::UnitChanged {
            obj: 0x010203,
            char_id: 34,
            kicker: "".into(),
            name: "Bo".into(),
            from: 0,
            to: 5,
            expel: false,
        }
    );
    let expel = cat(&[
        &[0, 0, 0],
        &le32(34),
        &st("Anna"),
        &st("Bo"),
        &le32(5),
        &le32(0),
        &[1],
    ]);
    match event(op::SCUnitExpeditionChangedPacket, &expel) {
        GuildEvent::UnitChanged {
            obj,
            from,
            to,
            expel,
            kicker,
            ..
        } => assert_eq!((obj, from, to, expel, &*kicker), (0, 5, 0, true, "Anna")),
        other => panic!("{other:?}"),
    }
    assert_eq!(op::SCUnitExpeditionChangedPacket, 0x01c);
}

#[test]
#[ignore = "1.2 byte layout; the 10.x layouts are checked against captured packets (tests/capture_10x.rs)"]
fn roles_owner_and_dismissal_are_read() {
    // SCExpeditionRoleChangedPacket: u32 id, u8 role, str name
    assert_eq!(
        event(
            op::SCExpeditionRoleChangedPacket,
            &cat(&[&le32(34), &[2], &st("Bo")])
        ),
        GuildEvent::RoleChanged {
            char_id: 34,
            role: 2,
            name: "Bo".into()
        }
    );
    // SCExpeditionOwnerChangedPacket: u32 id, u32 id2, str name
    assert_eq!(
        event(
            op::SCExpeditionOwnerChangedPacket,
            &cat(&[&le32(33), &le32(34), &st("Bo")])
        ),
        GuildEvent::OwnerChanged {
            from: 33,
            to: 34,
            name: "Bo".into()
        }
    );
    // SCExpeditionDismissedPacket: u32 id, bool success
    assert_eq!(
        event(op::SCExpeditionDismissedPacket, &cat(&[&le32(5), &[1]])),
        GuildEvent::Dismissed {
            guild: 5,
            success: true
        }
    );
    assert_eq!(op::SCExpeditionRoleChangedPacket, 0x00d);
    assert_eq!(op::SCExpeditionOwnerChangedPacket, 0x00e);
    assert_eq!(op::SCExpeditionDismissedPacket, 0x015);
}

#[test]
#[ignore = "1.2 byte layout; the 10.x layouts are checked against captured packets (tests/capture_10x.rs)"]
fn other_packets_and_other_levels_are_not_guild_packets() {
    let body = cat(&[&le32(5), &[1]]);
    assert_eq!(parse(LEVEL_GAME, op::SCChatMessagePacket, &body), Ok(None));
    assert_eq!(parse(2, op::SCExpeditionDismissedPacket, &body), Ok(None));
}

#[test]
#[ignore = "1.2 byte layout; the 10.x layouts are checked against captured packets (tests/capture_10x.rs)"]
fn a_short_or_lying_packet_is_an_error_not_a_panic() {
    // a member list that claims 200 members but carries none
    let body = cat(&[&le32(200), &[200], &le32(5)]);
    assert!(parse(LEVEL_GAME, op::SCExpeditionMemberListPacket, &body).is_err());
    // truncated invitation
    let body = cat(&[&le32(33), &st("Anna")]);
    assert!(parse(LEVEL_GAME, op::SCExpeditionInvitationPacket, &body).is_err());
    // empty bodies
    for op in [
        op::SCFactionCreatedPacket,
        op::SCExpeditionMemberStatusChangedPacket,
        op::SCUnitExpeditionChangedPacket,
        op::SCExpeditionRoleChangedPacket,
        op::SCExpeditionOwnerChangedPacket,
        op::SCExpeditionDismissedPacket,
    ] {
        assert!(parse(LEVEL_GAME, op, &[]).is_err(), "opcode {op:#x}");
    }
}

#[test]
#[ignore = "1.2 byte layout; the 10.x layouts are checked against captured packets (tests/capture_10x.rs)"]
fn the_activity_parser_hands_guild_packets_on() {
    let body = cat(&[&le32(5), &[1]]);
    match crate::sc::parse(LEVEL_GAME, op::SCExpeditionDismissedPacket, &body) {
        Ok(Some(ServerEvent::Activity(ActivityEvent::Guild(g)))) => {
            assert_eq!(
                *g,
                GuildEvent::Dismissed {
                    guild: 5,
                    success: true
                }
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
#[ignore = "1.2 byte layout; the 10.x layouts are checked against captured packets (tests/capture_10x.rs)"]
fn the_roles_and_the_founding_numbers_follow_the_server() {
    assert!(!role::can_invite(role::APPRENTICE));
    assert!(!role::can_invite(role::MEMBER));
    assert!(role::can_invite(role::OFFICER));
    assert!(role::can_invite(role::COMMANDER));
    assert!(role::can_invite(role::OWNER));
    assert_eq!(role::name(255), "General");
    assert_eq!(role::name(2), "Officer");
    assert_eq!((CREATE_COST, CREATE_LEVEL, CREATE_TEAM), (10_000, 5, 5));
    for code in [293, 294, 295, 296, 297, 299, 321, 330, 761] {
        assert!(err::is_founding(code), "{code}");
    }
    assert!(!err::is_founding(311));
    assert!(!err::is_founding(0));
}
