# aaemu-playerbots10

**Headless ArcheAge 10.0.2.13 player bots for the [AAEmu 10.x](https://github.com/AAEmu/AAEmu-10.x)
server.** A port of Nick Messer's
[aaemu-playerbots](https://github.com/NickMesser/aaemu-playerbots), which was written for the
ArcheAge 1.2 client and the AAEmu-1.2-Rust server, to the 10.0.2.13 client protocol.

Every bot is an ordinary network client: it logs in through the login server, creates or selects a
character and plays over the normal game protocol. The server needs no changes. The bots quest,
fight, trade, travel and report the server bugs they run into; the behaviour, the goals, the web UI
and the issue reports are Nick's work and are described in
[docs/ORIGINAL-README.md](docs/ORIGINAL-README.md) and [docs/OPERATING.md](docs/OPERATING.md).
This repository changes what is needed to speak 10.x and to read 10.x game data.

## Status

Work in progress. What has been run against a live AAEmu 10.x server:

| | |
| --- | --- |
| Login | Launcher web auth with the V2 challenge answer; account auto-creation (`AutoAccount`) |
| Lobby | RSA key exchange, encrypted game channel, connection states, character list, create, select, spawn |
| In the world | Movement on the navmesh with terrain heights, NPC interaction, quest accept / objectives / turn-in, combat and grinding, levelling |

What is ported but **not yet tested live**: duels, player trade, parties, dungeons, housing, trade
runs, farming, mail, merchants and gear, portals and the teleport book, mounts and gliders, boats,
guilds, world events. Their packets follow the 10.x server's readers and writers; expect fixes once
they are exercised.

What **does not work** yet:

* **Auction house.** The 10.x auction lot carries a full item record and the requests no longer
  name the auctioneer; posting, searching and the price lookup are ported, bids and cancellations
  are not and are never sent. Keep `[market] auction = false` (the example config does).
* **Packets the 10.x client does not have.** The 1.2 opcode table is kept so the bot logic
  compiles, but packets without a 10.x opcode (they carry `0xfff`, e.g. `CSSetForceAttackPacket`)
  are never put on the wire, so the features behind them do nothing.
* **Spawn data from the server.** The 10.x World keeps no `npc_spawns*.json` / `doodad_spawns*.json`;
  the bots need them to find NPCs and doodads, so they are built from the client's level data (see
  below). Spawners that only an event activates are listed like any other.
* Each race starts in its own zone; a bot is logged out again when the zone host of its start zone is
  not running. Restrict `[bots] races` to the zones your server runs.

## What changed against the 1.2 version

* **Transport** (`crates/proto/src/game.rs`, `crypto/`): the RSA-1024 key exchange of
  `X2EnterWorldResponse` / `CSAesXorKey`, the C→S cipher (AES-128-CBC plus the sequenced XOR
  keystream) and the S→C stream cipher, as AAEmu's `EncryptionManager` implements them; level-5
  frames are delivered as ordinary game packets.
* **Login** (`login.rs`): `CARequestWebAuth` with the launcher JSON, `ACChallenge2` answered with the
  V2 response, the 10.x world list and cookie layouts.
* **Lobby** (`crates/playerbots/src/bot/session.rs`): enter world → key exchange → states 0 and 1 →
  character list and state 2 → select or create → states 3..7 → `CSSpawnCharacter` +
  `CSNotifyInGame` → own unit state → `CSNotifyInGameCompleted`.
* **Opcodes** (`opcodes.rs`): generated from the AAEmu 10.x `CSOffsets` / `SCOffsets` / `PPOffsets`
  with `tools/gen_opcodes.py`.
* **Packet layouts**: positions are 11 bytes; ids that grew to `u64`, packed integers, the 10.x item
  record (variable equipment details, longer tail), equipment masks, unit state, character state,
  item tasks, quests, skills, buffs, doodads, chat, mail, trade, duels (sent to each side and naming
  only the opponent: `SELF_OBJ` marks the receiver), parties, housing and guilds. The client packets
  the bots send follow the 10.x readers, plus the bytes the real 10.x client adds (a trailing byte on
  every move, the input direction at the end of every skill cast).
* **World data** (`crates/world`): the queries read both the 1.2 and the 10.x `compact.sqlite3`
  (prices in `item_prices`, garden radius in `housing_sizes`, regrade odds in `item_enchant_ratios`,
  ...); `sql::has_column` picks the form. The test fixtures keep the 1.2 schema.

### How the layouts were checked

`crates/proto/examples/check_capture.rs` runs every server packet parser over packets captured from
a 10.x World talking to a real client and reports parse errors and unread bytes per opcode. All 61
packet types the bots parse that occurred in the captures read exactly. `tests/capture_10x.rs` does
the same as a test when `AA_CAPTURE_FRAMES` points at such a dump (records of
`u8 level, u16 opcode, u32 len, body`; captures hold player data, so none is included). Packets
that did not occur in the captures were compared with the AAEmu 10.x writers by hand.

## Quick start

You need:

* Rust 1.88 or newer.
* A running AAEmu 10.x server (login, world and the zone hosts of the zones the bots play in) whose
  login server creates unknown accounts (`AutoAccount`).
* Your own, legally obtained ArcheAge 10.0.2.13 client: the bake reads its `game_pak`, the spawn
  export its level data.
* The 10.x `compact.sqlite3` (decrypted) and the World's `Data` directory (for `CharTemplates.json`
  and `Portal/`).
* Python 3 for the spawn export. Reading files that are only in the pak uses the client's own
  `Bin64/xlpack.dll`, so that part needs Windows; loose files in `<client>/game` are read directly.

```sh
# 1. build
cargo build --release -p playerbots

# 2. bake terrain heights, navmeshes, zones and housing areas from your client, once
playerbots bake --pak <client>/game_pak --out <baked dir> --compact <compact.sqlite3>

# 3. build the bot data directory: retail NPC spawners and map doodads from the level data,
#    plus CharTemplates.json and Portal/ copied from the World's Data directory
python tools/export_spawns.py <client> <compact.sqlite3> <bot data dir> main_world
python tools/export_doodads.py <client> <bot data dir> main_world

# 4. write and edit the configuration
playerbots example-config > playerbots.toml

# 5. start the bots; Ctrl+C logs them out
playerbots run --config playerbots.toml
```

In `playerbots.toml` set `[server] login`, and in `[data]` `compact`, `server_data` (the bot data
directory) and `baked`. The web UI is at `http://127.0.0.1:8790/`. Everything else (GM bootstrap,
the issue reports, the optional LLM, the population director) works as described in
[docs/OPERATING.md](docs/OPERATING.md); where that document talks about the 1.2 server, read it as
background.

`tools/check_sql.py <compact.sqlite3>` runs every query of the world crate against a database and
lists the ones that fail: the first thing to run when a server ships a newer database.

## Development

```sh
cargo test --workspace
AA_CAPTURE_FRAMES=<dump> cargo test -p aa-proto --test capture_10x
cargo run -p aa-proto --example check_capture -- <dump>
```

The auction tests and the 1.2 byte-layout tests of the guild module are marked ignored with the
reason; the 1.2 byte-pinned protocol test files of the original were replaced by the capture test.

## Credits

* **Nick Messer** wrote [aaemu-playerbots](https://github.com/NickMesser/aaemu-playerbots): the bot
  runtime, every goal, the navigation, the bake, the web UI, the issue reports and the 1.2 protocol
  this port started from. The history of this repository starts with his.
* The behaviour of the bots, their class builds and the skill rotations come from
  [NulightJens/ArcheAge-PlayerBots](https://github.com/NulightJens/ArcheAge-PlayerBots) (see
  `crates/playerbots/data/rotations/NOTICE`).
* The wire protocol and packet layouts follow the code of [AAEmu](https://github.com/AAEmu)
  (GPL-3.0), for 10.x in particular [AAEmu-10.x](https://github.com/AAEmu/AAEmu-10.x).

## Disclaimer

This is an unofficial fan project. It is not affiliated with or endorsed by XLGAMES, Kakao Games,
Trion Worlds or Gamigo. It contains no game assets: you need your own legally obtained client and
the AAEmu server data. ArcheAge is a trademark of its respective owners. The bots log in and play
like real clients, so run them against servers you operate or have permission to test.

## License

GPL-3.0-or-later, like the project it is based on. See [LICENSE](LICENSE).
