"""Export the 10.x retail NPC spawners of a world into the playerbots spawn-file format.

Source: worlds/<world>/level_design/zone/<id>/zone_server/npc_spawners.g of every zone in
world.xml, loose from the client's game folder when present, else from the client pak (through
the client's own xlpack.dll). Spawner positions are zone-local: world = local + origin * 1024,
with the zone's originX/originY from world.xml. spawnerType -> npc_spawner_npcs.member_id (Npc).

Output: <out>/Worlds/<world>/npc_spawns_retail.json (UnitId, Position {X,Y,Z}, Zone, Spawner).
No NpcSpawnerIds: the bots read entries that carry them as event-only spawners.

Usage: python -I export_spawns.py <client root> <compact.sqlite3> <out data dir> [world]
"""
import ctypes as c, json, os, re, sqlite3, sys
import xml.etree.ElementTree as ET
from pathlib import Path

client, compact, out = Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3])
world = sys.argv[4] if len(sys.argv) > 4 else 'main_world'
CELL = 1024.0


class Pak:
    def __init__(self, root):
        os.add_dll_directory(str(root / 'Bin64'))
        d = c.WinDLL(str(root / 'Bin64' / 'xlpack.dll'))
        def fn(n, a, r):
            f = getattr(d, n); f.argtypes = a; f.restype = r; return f
        if not fn('?CreateFileSystem@@YA_NXZ', [], c.c_bool)():
            raise RuntimeError('xlpack file system unavailable')
        self.open = fn('?FOpen@@YAPEAUFile@@PEBD0@Z', [c.c_char_p, c.c_char_p], c.c_void_p)
        self.size = fn('?FSize@@YA_JPEAUFile@@@Z', [c.c_void_p], c.c_int64)
        self.read_ = fn('?FRead@@YA_JPEAUFile@@PEAD_J@Z', [c.c_void_p, c.c_void_p, c.c_int64], c.c_int64)
        self.close = fn('?FClose@@YAXAEAPEAUFile@@@Z', [c.POINTER(c.c_void_p)], None)
        mount = fn('?Mount@@YAPEAXPEBD0_N@Z', [c.c_char_p, c.c_char_p, c.c_bool], c.c_void_p)
        if not mount(b'/', str(root / 'game_pak').encode(), False):
            raise RuntimeError('cannot mount the client pak')

    def read(self, name):
        f = self.open(('game/' + name).encode(), b'rb')
        if not f:
            return None
        try:
            size = self.size(f); buf = c.create_string_buffer(size)
            if self.read_(f, buf, size) != size:
                raise RuntimeError('short pak read: ' + name)
            return buf.raw
        finally:
            h = c.c_void_p(f); self.close(c.byref(h))


POS = re.compile(r'pos \( x (\S+), y (\S+), z (\S+) \)')


def spawners(text):
    """(spawnerId, spawnerType, [positions]) per block; point spawners list their points, area and
    path spawners their first point (the bots only need a place to look)."""
    out, cur = [], None
    for line in text.splitlines():
        if line.rstrip() == 'spawner':
            cur = {'id': None, 'type': None, 'pos': []}; out.append(cur); continue
        if cur is None:
            continue
        s = line.strip()
        if s.startswith('spawnerId '):
            cur['id'] = int(s.split()[1])
        elif s.startswith('spawnerType '):
            cur['type'] = int(s.split()[1])
        else:
            m = POS.search(s)
            if m:
                cur['pos'].append(tuple(float(v) for v in m.groups()))
    return [b for b in out if b['id'] is not None and b['type'] is not None]


db = sqlite3.connect(f'file:{compact}?mode=ro', uri=True)
members = {}
for sid, mid in db.execute("SELECT npc_spawner_id, member_id FROM npc_spawner_npcs WHERE member_type = 'Npc'"):
    members.setdefault(sid, []).append(mid)

game = client / 'game'
root = ET.parse(game / 'worlds' / world / 'world.xml').getroot()
pak = None
rows, stats = [], {'zones': 0, 'loose': 0, 'pak': 0, 'missing': 0, 'spawners': 0, 'no_members': 0}
for z in root.findall('ZoneList/Zone'):
    zid = int(z.attrib['id']); ox = int(z.attrib['originX']) * CELL; oy = int(z.attrib['originY']) * CELL
    rel = f'worlds/{world}/level_design/zone/{zid}/zone_server/npc_spawners.g'
    stats['zones'] += 1
    p = game / rel
    if p.exists():
        data = p.read_bytes(); stats['loose'] += 1
    else:
        pak = pak or Pak(client)
        data = pak.read(rel)
        if data is None:
            stats['missing'] += 1; continue
        stats['pak'] += 1
    for b in spawners(data.decode('ascii', 'replace')):
        stats['spawners'] += 1
        mids = members.get(b['type'])
        if not mids:
            stats['no_members'] += 1; continue
        for (x, y, zz) in b['pos'][:1] if len(b['pos']) > 8 else b['pos']:
            for mid in mids:
                rows.append({'UnitId': mid, 'Position': {'X': round(x + ox, 3), 'Y': round(y + oy, 3), 'Z': round(zz, 3)},
                             'Zone': zid, 'Spawner': b['type']})

dest = out / 'Worlds' / world
dest.mkdir(parents=True, exist_ok=True)
(dest / 'npc_spawns_retail.json').write_text(json.dumps(rows), encoding='utf-8')
print(stats, 'entries', len(rows), '->', dest / 'npc_spawns_retail.json')
