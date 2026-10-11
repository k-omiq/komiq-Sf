"""Stand-in for komika-server in the process-management tests (src/local/tests.rs).

Usage: fake_server.py MODE. It records its environment and pid in its working
directory (the data dir), then behaves per MODE:
  setup     print a setup-code line, serve /health and `setupRequired: true`
  ready     no code line, serve /health and `setupRequired: false` (set-up server)
  crash     print an error and exit 3 before listening
  crash1    crash on the first run in this data dir, then behave like `ready`
  port      print the OS's "address in use" error and exit 1
  hang      never listen
  flaky     like `ready`, but exit 1 second after it starts listening
Modes that print the real server's `sqlite import:` lines first (the shapes are those of
apps/server/src/pg_import/; see src/import.rs):
  import-noop        checked, nothing to convert (warning), then never listen
  import-noop-ready  checked, nothing to convert (warning), then behave like `ready`
  import-prepare     checking, then never listen
  import-prepare-ready  checking, no further import line, then behave like `ready`
  import-stall       a conversion that prints one stage line and then nothing, for ever
  import-error       a conversion that reports an error and exits 1
  import-error-hang  the same, but the process stays alive after the error line
  import-ok          a conversion that takes about 3 seconds, a line every 0.3 s, then `ready`
  import-done-hang   a finished conversion, then never listen
  import-scan-ready  a finished import with a work copy left over: checking, the scan
                     line, two seconds of reading, a warning, then behave like `ready`
  import-scan-refused  a start that is refused after two kept copies were read: checking,
                     a scan line and 1.5 seconds of reading for each, one long error
                     line, exit 1
The setup code here is made up for the tests.
"""

import http.server
import json
import os
import sys
import threading
import time

MODE = sys.argv[1]
with open("fake-env.json", "w") as f:
    json.dump({k: v for k, v in os.environ.items() if k.isupper()}, f)
with open("fake-runs.log", "a") as f:
    f.write(f"{os.getpid()} {os.environ.get('BIND_ADDR')}:{os.environ.get('PORT')}\n")


def say(line):
    print(line, flush=True)


with open("fake-runs.log") as f:
    FIRST_RUN = len(f.readlines()) == 1

if MODE == "crash" or (MODE == "crash1" and FIRST_RUN):
    say("Error: database is locked")
    sys.exit(3)
if MODE == "port":
    say("Error: Address already in use (os error 98)")
    sys.exit(1)
if MODE == "hang":
    threading.Event().wait()

PREPARING = "sqlite import: preparing (checking source and target)"
NOOP = "sqlite import: warning: PostgreSQL already holds data or a completed import; leaving SQLite in place"
if MODE.startswith("import-"):
    say(PREPARING)
if MODE in ("import-noop", "import-noop-ready"):
    say(NOOP)
if MODE in ("import-noop", "import-prepare"):
    threading.Event().wait()
if MODE == "import-stall":
    say("sqlite import: source inventory start: 1060864 bytes")
    say("sqlite import: copy start")
    # Neither a warning nor a line of an unknown shape is progress.
    time.sleep(0.5)
    say("sqlite import: warning: unrecorded file /data/komika.sqlite3.import-work-old; leaving it alone")
    say("sqlite import: precheck failed: 0 failing rules")
    threading.Event().wait()
if MODE in ("import-error", "import-error-hang"):
    say("sqlite import: source inventory start: 1060864 bytes")
    say("sqlite import: repair start")
    say("sqlite import: repair: chapter: 2 rows would change")
    say("sqlite import: repair: chapter: 2 rows applied")
    say("sqlite import: repair complete: 0.119s")
    say("sqlite import: verify start")
    say("sqlite import: error: verify stage: verifier did not pass")
    if MODE == "import-error-hang":
        threading.Event().wait()
    say("Error: sqlite import: verify stage: verifier did not pass")
    sys.exit(1)
if MODE == "import-scan-ready":
    # startup.rs `settle_done`: the scan line, then whole files are read, then the warning.
    say("sqlite import: source inventory start: 1060864 bytes")
    time.sleep(2)
    say("sqlite import: warning: PostgreSQL already holds the imported data; leaving "
        + os.path.join(os.getcwd(), "komika.sqlite3")
        + " in place (earlier archive: komika.sqlite3.imported-20261011T115920Z)")
if MODE == "import-scan-refused":
    # startup.rs `kept_facts` and `refusal`: one scan line before each kept file whose
    # rows are counted, then the one error line with every path and the steps to take.
    source = "/Users/José Müller/Library/Application Support/komiq desktop/server/komika.sqlite3"
    kept = [source + ".import-kept-20261012T080000Z", source + ".import-kept-20261011T070000Z"]
    for size in (1060864, 2147483648):
        say(f"sqlite import: source inventory start: {size} bytes")
        time.sleep(1.5)
    work = ", ".join(
        source + name
        for name in (
            ".import-work.repair-lock", ".import-work", ".import-work-wal", ".import-work-shm",
            ".import-work-journal", ".import-work-source", ".import-work-source-wal",
            ".import-work-source-shm", ".import-work-source-journal", ".import-work-state.next-4f2a",
        )
    )
    facts = ["comments: 4, user_library: 12, users: 3, work: 15", "empty: no rows in users, user_library, comments, work"]
    listed = ", ".join(f"{file} ({fact})" for file, fact in zip(kept, facts))
    text = (
        f"recovery stage: the SQLite file {source} named by the unfinished import is missing, "
        "and PostgreSQL holds only part of its data. Nothing was changed. Kept by earlier starts "
        "(each is the SQLite database as it was when an earlier import attempt began): "
        f"{listed}. To continue, put {source} back and start again. Or, to import the kept copy "
        f"{kept[0]} ({facts[0]}) instead, rename {kept[0]} to {source} and start again; any other "
        "kept copy is imported the same way. Or, to start with an empty database, empty the "
        "PostgreSQL database (drop it and create it again), delete "
        f"{source}.import-work-state and the work files {work}, move {', '.join(kept)} out of the "
        "data directory and start again"
    )
    say("sqlite import: error: " + text)
    say("Error: sqlite import: " + text)
    sys.exit(1)
if MODE in ("import-ok", "import-done-hang"):
    lines = [
        "sqlite import: source inventory start: 1060864 bytes",
        "sqlite import: copy start",
        "sqlite import: disk space: 47126335488 bytes free, 276922368 bytes required",
        "sqlite import: copy complete: 0.164s",
        "sqlite import: migrate start",
        "sqlite import: import start",
        "sqlite import: table chapter: 250000 rows sent, 5.004s",
        "sqlite import: table users complete: 3 rows, 0.001s, 553 COPY bytes",
        "sqlite import: verify users: SQLite=3 PG=3 checksum 9f",
        "sqlite import: verifier PASS: 63 tables",
    ]
    for line in lines:
        say(line)
        if MODE == "import-ok":
            time.sleep(0.3)
    say("sqlite import: renamed original to " + os.path.join(os.getcwd(), "komika.sqlite3.imported-20261011T115920Z"))
    say("sqlite import: rename complete: 0.079s")
    say("sqlite import: complete: 3.151s total")
    if MODE == "import-done-hang":
        threading.Event().wait()

if MODE == "setup":
    say(
        "\x1b[33m WARN\x1b[0m komika_server::instance: no admin account yet: open the setup "
        "screen and enter setup code TEST-CODE-0042 (valid until the owner account is created)"
    )


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/health":
            self.reply(b"ok", "text/plain")
        else:
            self.send_error(404)

    def do_POST(self):
        self.rfile.read(int(self.headers.get("content-length") or 0))
        body = {"data": {"instance": {"setupRequired": MODE == "setup"}}}
        self.reply(json.dumps(body).encode(), "application/json")

    def reply(self, body, kind):
        self.send_response(200)
        self.send_header("content-type", kind)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


server = http.server.HTTPServer((os.environ["BIND_ADDR"], int(os.environ["PORT"])), Handler)
say(f"listening on http://{os.environ['BIND_ADDR']}:{os.environ['PORT']}/graphql")
if MODE == "flaky":
    threading.Timer(1.0, lambda: os._exit(1)).start()
server.serve_forever()
