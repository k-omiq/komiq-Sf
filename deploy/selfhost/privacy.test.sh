#!/usr/bin/env bash
# Offline context parser and documented-command argv checks. No operator env read.
set -euo pipefail
source_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
python3 - "$source_dir/../.." <<'PY'
import os, pathlib, re, shutil, stat, subprocess, tempfile, sys
repo=pathlib.Path(sys.argv[1]).resolve()
# Dockerfile-specific ignore takes precedence when present; otherwise root context.
assert 'context: ../..' in (repo/'deploy/selfhost/docker-compose.yml').read_text()
specific=repo/'deploy/selfhost/Dockerfile.dockerignore'
ignore=specific if specific.exists() else repo/'.dockerignore'
rules=[s.strip().strip('/') for s in ignore.read_text().splitlines() if s.strip() and not s.startswith('#')]
# Patterns used here are glob/recursive path exclusions; reject unsupported rules
# rather than claiming our parser covers Docker's full pattern language.
assert not any(r.startswith('!') or '[' in r for r in rules)
def regex(rule):
    # Docker globs anchor at the context root. * stays within one component;
    # **/ matches zero or more directories. Parent exclusions cover descendants.
    result=''; i=0
    while i<len(rule):
        if rule.startswith('**/',i): result+='(?:.*/)?'; i+=3
        elif rule.startswith('**',i): result+='.*'; i+=2
        elif rule[i]=='*': result+='[^/]*'; i+=1
        elif rule[i]=='?': result+='[^/]'; i+=1
        else: result+=re.escape(rule[i]); i+=1
    return re.compile('^'+result+'$')
patterns=[regex(r) for r in rules]
def excluded(path):
    parts=path.split('/')
    ancestors=['/'.join(parts[:n]) for n in range(1,len(parts)+1)]
    return any(p.fullmatch(a) for p in patterns for a in ancestors)
# Distinguish root-only wildcard rules from recursive exclusions.
assert regex('*.dump').fullmatch('root.dump')
assert not regex('*.dump').fullmatch('nested/private.dump')
assert regex('**/*.dump').fullmatch('nested/deep/private.dump')
assert regex('**/*.dump').fullmatch('root.dump')
fixtures=['komiq.dump','nested/private.dump','deploy/selfhost/komiq-server-data.tgz',
          'nested/volume.tgz','deploy/selfhost/backups/opaque-private-file',
          'deploy/selfhost/.sqlite-image-komiq','deploy/selfhost/.env',
          'deploy/selfhost/.env.bak','deploy/selfhost/.env.password.fixture',
          '.env','nested/.env']
context=[p for p in fixtures+['deploy/selfhost/Dockerfile','apps/server/src/main.rs'] if not excluded(p)]
assert context == ['deploy/selfhost/Dockerfile','apps/server/src/main.rs'], context
print('PASS: actual installer context ignore excludes backup and env fixtures')
page=(repo/'apps/docs/src/content/self-host/docker.md').read_text()
procedure=re.search(r'For a new installation without the installer:\n\n```sh\n(.*?)```', page,re.S)[1]
with tempfile.TemporaryDirectory() as d:
    root=pathlib.Path(d); (root/'deploy/selfhost').mkdir(parents=True)
    (root/'deploy/selfhost/.env.example').write_text('TZ=UTC\nKOMIQ_DB_PASSWORD=\nCUSTOM=keep\n')
    bin=root/'bin'; bin.mkdir()
    # Synthetic secret only. Every external process records its received argv.
    secret='d'*64
    tools=['cp','openssl','sed','awk','mv','chmod','docker','tr']
    for tool in tools:
        real=shutil.which(tool)
        if tool not in ['docker', 'openssl']: assert real, tool
        code='#!/bin/sh\nprintf "%s\\n" "$*" >> "$ARGV_LOG"\n'
        if tool=='openssl': code += 'printf "%s\\n" '+secret+'\n'
        elif tool=='docker': code += 'exit 0\n'
        else: code += 'exec '+real+' "$@"\n'
        (bin/tool).write_text(code); (bin/tool).chmod(0o755)
    env=dict(os.environ,PATH=str(bin)+':'+os.environ['PATH'],ARGV_LOG=str(root/'argv'))
    # POSIX shell procedure runs unchanged in both default bash and sh.
    for shell in ['bash','sh']:
        subprocess.run([shell,'-c',procedure],cwd=root,env=env,check=True)
        data=(root/'deploy/selfhost/.env').read_text()
        assert data.count('KOMIQ_DB_PASSWORD=')==1 and 'KOMIQ_DB_PASSWORD='+secret in data
        assert 'TZ=UTC' in data and 'CUSTOM=keep' in data
        assert secret not in (root/'argv').read_text(), 'password in process argv'
        assert stat.S_IMODE((root/'deploy/selfhost/.env').stat().st_mode)==0o600
print('PASS: documented manual password procedure keeps password out of argv')
PY
