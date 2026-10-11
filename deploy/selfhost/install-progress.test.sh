#!/usr/bin/env bash
# Check real server log shapes against the installer's durable progress cursor.
set -euo pipefail
source_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
python3 - "$source_dir/install-progress.awk" <<'PY'
import subprocess, sys
marks=''
def sample(line, expected):
    global marks
    result=subprocess.check_output(['awk','-v','marks='+marks,'-f',sys.argv[1]],input=line+'\n',text=True)
    advanced, marks=result.split('\n',1)
    assert advanced==str(expected), (line,advanced,expected)
sample('sqlite import: copy start',1)
sample('sqlite import: copy start',0)
sample('sqlite import: copy complete: 3.000s',1)
sample('sqlite import: copy complete: 9.000s',0)
sample('sqlite import: precheck users start',1)
sample('sqlite import: precheck users complete: 10 rows',1)
sample('sqlite import: table users: 10 rows sent, 5.000s',1)
sample('sqlite import: table users: 10 rows sent, 50.000s',0)
sample('sqlite import: table users: 9 rows sent, 50.000s',0)
sample('sqlite import: table users: 11 rows sent, 55.000s',1)
sample('',0) # entire log rotates away
sample('sqlite import: copy start\nsqlite import: table users: 10 rows sent, 5.000s',0)
sample('sqlite import: table users complete: 11 rows, 55.000s, 100 COPY bytes',1)
sample('sqlite import: table users complete: 11 rows, 70.000s, 101 COPY bytes',1)
sample('sqlite import: table users complete: 11 rows, 90.000s, 100 COPY bytes',0)
sample('sqlite import: table chapters: 1 rows sent, 5.000s',1)
sample('sqlite import: verify users: SQLite=11 PG=11 checksum PASS',1)
sample('sqlite import: verify users: SQLite=11 PG=11 checksum PASS',0)
sample('sqlite import: repair: users: 3 rows applied',1)
sample('sqlite import: repair: users: 3 rows applied',0)
sample('sqlite import: renamed original to komika.sqlite3.imported-20261011T000000Z',1)
sample('sqlite import: renamed original to komika.sqlite3.imported-20261011T000001Z',0)
sample('sqlite import: disk space: 100 bytes free, 10 bytes required',0)
print('PASS: stage, row and COPY-byte high waters survive duplicates and rotation')
PY
