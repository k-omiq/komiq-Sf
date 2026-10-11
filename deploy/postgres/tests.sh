#!/bin/sh
set -eu
cd "$(dirname "$0")"
for script in *.sh; do sh -n "$script"; done
for module in *.py; do python3 -B -c 'import ast, sys; ast.parse(open(sys.argv[1]).read(), sys.argv[1])' "$module"; done
# test_rehearsal drives rehearsal.sh against a daemon-free fake docker (about eight minutes).
# test_runbook runs the pre-flight and removal blocks of RESTORE.md with bash and stand-in commands.
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest -v test_hosted test_drill test_health test_image test_safety test_runbook test_cutover test_rehearsal
