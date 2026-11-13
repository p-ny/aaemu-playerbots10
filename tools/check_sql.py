"""Run every SQL string literal of the world/bake crates against the 10.x compact.sqlite3 and
report the failing ones (missing tables/columns)."""
import glob, re, sqlite3, sys

# Usage: python check_sql.py <compact.sqlite3>
from pathlib import Path
DB = sys.argv[1]
ROOT = Path(__file__).resolve().parents[1]
db = sqlite3.connect(f'file:{DB}?mode=ro', uri=True)
pat = re.compile(r'(?:r#?"|")(\s*(?:SELECT|WITH)\b.*?)(?:"#?)', re.S | re.I)
bad = 0
for path in sorted(glob.glob(str(ROOT / 'crates/*/src/**/*.rs'), recursive=True)):
    src = open(path, encoding='utf-8').read()
    for m in pat.finditer(src):
        sql = m.group(1)
        sql = re.sub(r'\\\s*\n\s*', ' ', sql)  # rust line continuations
        if '{' in sql:
            sql = re.sub(r'\{[^}]*\}', 'id', sql)
        q = re.sub(r'\?\d*', 'NULL', sql)
        try:
            db.execute(q + ('' if 'limit' in q.lower() else ' LIMIT 0')).fetchall()
        except Exception as e:
            bad += 1
            line = src.count('\n', 0, m.start()) + 1
            print(f'{path.split("crates")[-1]}:{line}: {e}\n    {" ".join(sql.split())[:220]}')
print('failing:', bad)
