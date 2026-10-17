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
The setup code here is made up for the tests.
"""

import http.server
import json
import os
import sys
import threading

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
