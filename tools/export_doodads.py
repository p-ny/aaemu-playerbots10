"""Export the level-pack map doodads of a world into the playerbots spawn-file format.

Source: worlds/<world>/level_design/cells/<CX>_<CY>/doodad.g (loose in the client's game folder,
else the client pak), the file our World plants map doodads from (ZoneDoodadPlacementCatalog):
blocks with `type <template>` and `pos ( x, y, z )`, cell-local; world = cell * 1024 + local.

Output: <out>/Worlds/<world>/doodad_spawns_levelpack.json (UnitId, Position {X,Y,Z}).
Usage: python -I export_doodads.py <client root> <out data dir> [world]
"""
import json, re, sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
client, out = Path(sys.argv[1]), Path(sys.argv[2])
world = sys.argv[3] if len(sys.argv) > 3 else 'main_world'

TYPE = re.compile(r'\btype\s+(\d+)')
POS = re.compile(r'pos\s*\(\s*x\s+([-\d.eE+]+)\s*,\s*y\s+([-\d.eE+]+)\s*,\s*z\s+([-\d.eE+]+)\s*\)')
CELL = re.compile(r'^(\d+)_(\d+)$')

game = client / 'game'
cells = sorted({p.name for p in (game / 'worlds' / world / 'cells').iterdir() if CELL.match(p.name)} |
               {p.name for p in (game / 'worlds' / world / 'level_design' / 'cells').iterdir() if CELL.match(p.name)})
pak = None
rows, stats = [], {'cells': len(cells), 'loose': 0, 'pak': 0, 'none': 0, 'doodads': 0}
for name in cells:
    cx, cy = (int(v) for v in CELL.match(name).groups())
    rel = f'worlds/{world}/level_design/cells/{name}/doodad.g'
    p = game / rel
    if p.exists():
        data = p.read_bytes(); stats['loose'] += 1
    else:
        if pak is None:
            from export_spawns_pak import Pak
            pak = Pak(client)
        data = pak.read(rel)
        if data is None:
            stats['none'] += 1; continue
        stats['pak'] += 1
    text = data.decode('ascii', 'replace')
    # one block per doodad: split where a new "doodad" block starts
    for block in re.split(r'(?m)^doodad\s*$', text):
        t = TYPE.search(block); q = POS.search(block)
        if not t or not q:
            continue
        x, y, z = (float(v) for v in q.groups())
        rows.append({'UnitId': int(t.group(1)), 'Position': {'X': round(cx * 1024 + x, 3), 'Y': round(cy * 1024 + y, 3), 'Z': round(z, 3)}})
        stats['doodads'] += 1

dest = out / 'Worlds' / world
dest.mkdir(parents=True, exist_ok=True)
(dest / 'doodad_spawns_levelpack.json').write_text(json.dumps(rows), encoding='utf-8')
print(stats, '->', dest / 'doodad_spawns_levelpack.json')
