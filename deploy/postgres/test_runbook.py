"""Run the pre-flight and removal blocks of RESTORE.md against stand-in commands.

The blocks are what an operator pastes on the production host. This test takes
them from the document itself and runs them with bash, with `docker`, `stat`,
`df`, `sudo` and a few more replaced by small stand-ins on PATH, and with the
two fixed host paths (/srv and /run/lock) moved below a temporary directory.
It proves the control flow of the blocks: a failed check stops its block and
every later one, nothing is created or removed after a failure, each `rm`
runs only as the last link of its identity check, and a command that fails
and prints nothing never counts as a passed check. It does not prove that the
real commands print what the stand-ins print; that is for the first run on a
test host.
"""
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import sqlite3
import subprocess
import tarfile
import tempfile
import textwrap
import unittest

KIT = Path(__file__).resolve().parent
RUN = "phase5a-r5"

STAND_IN = r'''#!/usr/bin/env python3
import hashlib, json, os, stat, sys
name, args = os.path.basename(sys.argv[0]), sys.argv[1:]
here = os.path.dirname(os.path.abspath(sys.argv[0]))
state = json.load(open(os.path.join(here, "state.json")))
with open(os.path.join(here, "calls.jsonl"), "a") as out:
    out.write(json.dumps([name] + args) + "\n")
plain = [arg for arg in args if arg != "--"]

def fmt():
    return args[args.index("--format") + 1] if "--format" in args else ""

if name == "docker":
    if state["docker_fails"]:
        sys.exit(1)
    if args[:2] == ["context", "show"]:
        print(state["context"])
    elif args[:2] == ["context", "inspect"]:
        print(state["endpoint"])
    elif args[:2] == ["compose", "version"]:
        print("Docker Compose version v5.5.1")
    elif args[:1] == ["info"]:
        print(state["docker_root"])
    elif args[:1] == ["stats"]:
        print("komika-server-1 100MiB / 1GiB 1.00% 10")
    elif args[:1] == ["ps"]:
        if any(arg.startswith("label=cc.komiq.rehearsal") for arg in args):
            print("\n".join(state["labelled"]), end="\n" if state["labelled"] else "")
        elif "-aq" in args:
            print("\n".join("id" + str(n) for n in range(len(state["containers"]))))
        else:
            print("\n".join(state["containers"]))
    elif args[:1] == ["inspect"]:
        if "println .Source" in fmt():
            print("\n".join(state["sources"]))
        elif ".State.StartedAt" in fmt():
            for n, container in enumerate(state["containers"]):
                print(f"id{n} /{container} {state['started']} 0 running {state['no_label']}")
        else:
            for container in state["containers"]:
                print("/" + container + " bind:/srv/other")
    elif args[:2] == ["volume", "ls"]:
        print("\n".join([] if "--filter" in args else state["volumes"]))
    elif args[:2] == ["volume", "inspect"]:
        print("komika_server-data local /var/lib/docker/volumes/komika_server-data/_data null null")
    elif args[:2] == ["network", "ls"]:
        print("\n".join([] if "--filter" in args else ["komika_komika"]))
    elif args[:2] == ["network", "inspect"]:
        print("komika_komika " + state["subnet"])
    elif args[:2] == ["image", "inspect"]:
        if not os.path.exists(os.path.join(here, "loaded")) and not state["image"]:
            sys.exit(1)
        print("sha256:imageid")
    elif args[:1] == ["load"]:
        sys.stdin.buffer.read()
        open(os.path.join(here, "loaded"), "w").close()
        print("Loaded image: komika-postgres:18.6-pgbackrest-2.59")
    else:
        sys.exit(64)
elif name == "stat":
    form, paths = args[args.index("-c") + 1], [arg for arg in plain if arg not in ("-c", args[args.index("-c") + 1])]
    for path in paths:
        if path == state["lock_directory"]:
            print(state["lock_directory_state"])
            continue
        info = os.stat(path)
        print(form.replace("%u", str(info.st_uid)).replace("%a", format(stat.S_IMODE(info.st_mode), "o"))
              .replace("%s", str(info.st_size)).replace("%n", path).replace("%d", str(info.st_dev)))
elif name == "df":
    print("Avail" if any(arg.startswith("--output") for arg in args) else "Filesystem 1B-blocks Used Available Use% Mounted on")
    for path in [arg for arg in plain if not arg.startswith("-")]:
        print(state["free"] if any(arg.startswith("--output") for arg in args) else f"/dev/x 1 1 {state['free']} 1% {path}")
elif name == "findmnt":
    if state["findmnt_fails"]:
        sys.exit(1)
    print("\n".join(state["mounted"]), end="\n" if state["mounted"] else "")
elif name == "sudo":
    if args == ["-n", "true"]:
        if not state["sudo"]:
            sys.stderr.write("sudo: a password is required\n")
            sys.exit(1)
    elif not state["sudo"]:
        sys.exit(1)  # The real one would prompt; a prompt must never be reached.
    else:
        os.execvp(args[0], args)
elif name == "mkdir":
    sys.exit(1)
elif name == "install":
    if "-d" in args:
        os.mkdir(plain[-1], 0o700)
    else:
        with open(plain[-2], "rb") as source, open(plain[-1], "wb") as target:
            target.write(source.read())
        os.chmod(plain[-1], 0o755)
elif name == "sha256sum":
    def digest(path):
        return hashlib.sha256(open(path, "rb").read()).hexdigest()
    if "-c" in args:
        failed = 0
        for line in open(plain[-1]).read().splitlines():
            wanted, path = line.split("  ", 1)
            good = digest(path) == wanted
            failed += not good
            print(f"{path}: {'OK' if good else 'FAILED'}")
        sys.exit(1 if failed else 0)
    for path in plain:
        print(f"{digest(path)}  {path}")
elif name == "flock":
    sys.exit(0 if state["lease"] else 1)
elif name == "realpath":
    print(os.path.realpath(plain[-1]))
elif name == "rm":
    import shutil
    for path in [arg for arg in plain if not arg.startswith("-")]:
        if os.path.isdir(path) and not os.path.islink(path):
            shutil.rmtree(path)
        else:
            os.unlink(path)
'''


def blocks():
    text = (KIT / "RESTORE.md").read_text()
    found = re.findall(r"^```sh\n(.*?)^```", text, flags=re.S | re.M)

    def one(start):
        chosen = [body for body in found if body.startswith(start)]
        assert len(chosen) == 1, start
        return chosen[0]

    return {"A": one("# A. "), "B": one("# B. "), "C": one("# C. "), "D": one("# D. "),
            "pitr": one('target="$root/pitr"'), "base": one("cd / \\\n")}


class RunbookTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.top = Path(self.temp.name).resolve()
        self.bin = self.top / "bin"
        self.bin.mkdir()
        for name in ("docker", "stat", "df", "findmnt", "sudo", "install", "sha256sum", "flock", "realpath", "rm"):
            (self.bin / name).write_text(STAND_IN)
            (self.bin / name).chmod(0o755)
        self.srv = self.top / "srv"
        self.srv.mkdir()
        self.lock_directory = self.top / "run-lock"
        self.lock_directory.mkdir()
        self.lease = self.lock_directory / "komika-pg-rehearsal-operator.lock"
        self.base = self.srv / f"komika-pg-{RUN}"
        self.docker_root = self.top / "docker"
        self.state = {
            "context": "default", "endpoint": "unix:///var/run/docker.sock", "docker_root": str(self.docker_root),
            "containers": ["komika-server-1", "komika-nginx-1"], "labelled": [], "volumes": ["komika_server-data"],
            "sources": [str(self.docker_root / "volumes" / "komika_server-data" / "_data"), "/etc/komika", ""],
            "subnet": "172.21.0.0/16", "image": False, "free": "80000000000", "mounted": ["/", "/srv-other"],
            "lease": True, "started": "2026-10-01T00:00:00Z", "no_label": "", "sudo": True,
            "docker_fails": False, "findmnt_fails": False,
            "lock_directory": str(self.lock_directory), "lock_directory_state": "0 drwxrwxrwt",
        }
        self.blocks = {name: self.moved(body) for name, body in blocks().items()}
        self.environment = {"PATH": str(self.bin) + os.pathsep + os.environ["PATH"], "HOME": str(self.top)}

    def moved(self, text):
        """The two fixed host paths of the runbook, below this test's directory."""
        self.assertNotIn(str(self.top), text)
        return text.replace("/srv", str(self.srv)).replace("/run/lock", str(self.lock_directory))

    def run_blocks(self, *names, before="", after="", **environment):
        (self.bin / "state.json").write_text(json.dumps(self.state))
        log = self.bin / "calls.jsonl"
        for left in (log, self.bin / "loaded"):  # A new host each time: no call log, no loaded image.
            if left.exists():
                left.unlink()
        script = before + "\n" + "\n".join(self.blocks[name] for name in names) + "\n" + after
        result = subprocess.run(["bash", "-c", script], env=dict(self.environment, **environment), cwd=str(self.top),
                                capture_output=True, text=True, stdin=subprocess.DEVNULL)
        self.calls = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
        self.output = result.stdout + result.stderr
        return result.stdout

    def called(self, *prefix):
        return [call for call in self.calls if call[:len(prefix)] == list(prefix)]

    def provision(self):
        """After A and B have passed in a script: put the artifacts and inputs in place."""
        return textwrap.dedent(f"""
            cp -R {self.top}/staged/. "$base"/
            chmod 600 "$env_file"
        """)

    def stage(self):
        """Build the transfer directory once, outside the run's directory."""
        self.run_blocks("A", "B")
        self.assertIn("gate=B (B expected)", self.output)
        transfer = self.base / "transfer"
        kit = self.top / "postgres"
        kit.mkdir()
        (kit / "rehearsal.sh").write_text("#!/bin/sh\n")
        with tarfile.open(transfer / "komika-postgres-kit.tar.gz", "w:gz") as archive:
            archive.add(kit, arcname="postgres")
        shutil.rmtree(kit)
        image = self.top / "komika-postgres-image.tar"
        image.write_bytes(b"image layers" * 1000)
        subprocess.run(["gzip", str(image)], check=True)
        shutil.move(str(image) + ".gz", transfer / "komika-postgres-image.tar.gz")
        (transfer / "komika-server").write_bytes(b"\x7fELF")
        (transfer / "image-id.txt").write_text("sha256:imageid\n")
        names = ("komika-postgres-image.tar.gz", "komika-postgres-kit.tar.gz", "komika-server", "image-id.txt")
        (transfer / "SHA256SUMS").write_text("".join(
            f"{hashlib.sha256((transfer / name).read_bytes()).hexdigest()}  {name}\n" for name in names))
        (self.base / "private" / "rehearsal.env").write_text(
            "KOMIKA_REHEARSAL_PROJECT=x\nKOMIKA_POSTGRES_PASSWORD=never-printed-value\n")
        # A real database: block C reads the migration history of the copy.
        copy = sqlite3.connect(self.base / "input" / "copy.sqlite3")
        copy.execute("CREATE TABLE _sqlx_migrations (version INTEGER PRIMARY KEY, success BOOLEAN NOT NULL)")
        copy.executemany("INSERT INTO _sqlx_migrations VALUES (?, 1)", [(121,), (122,)])
        copy.commit()
        copy.close()
        self.assertEqual((self.base / "input" / "copy.sqlite3").stat().st_size, 8192)
        staged = self.top / "staged"
        staged.mkdir()
        for name in ("transfer", "private", "input"):
            shutil.copytree(self.base / name, staged / name)
        shutil.rmtree(self.base)

    def test_the_four_blocks_pass_in_order_and_each_needs_the_one_before(self):
        self.stage()
        out = self.run_blocks("A", "B", before="", after="")
        self.assertIn("BLOCK A OK", out)
        self.assertIn("BLOCK B OK", out)
        shutil.rmtree(self.base)
        script_c_d = self.provision() + self.blocks["C"] + self.blocks["D"] + 'echo "final gate=$gate cwd=$PWD"\n'
        out = self.run_blocks("A", "B", after=script_c_d)
        for letter in "ABCD":
            self.assertIn(f"BLOCK {letter} OK", out, self.output)
            self.assertIn(f"gate={letter} ({letter} expected)", out)
        self.assertNotIn("STOP", self.output)
        self.assertIn(f"final gate=D cwd={self.base}/kit", out)
        self.assertNotIn("never-printed-value", self.output)  # The env file's names only.
        self.assertIn("KOMIKA_POSTGRES_PASSWORD\n", out)
        self.assertEqual(len(self.called("docker", "load")), 1)
        self.assertTrue((self.base / "kit" / "rehearsal.sh").is_file())
        self.assertFalse((self.base / "transfer" / "komika-postgres-image.tar.gz").exists())
        self.assertEqual(sorted(os.listdir(self.base / "records")),
                         ["input-hashes.txt", "others-after-load.txt", "others-before.txt", "r2-metrics-before-time.txt"])
        self.assertEqual(self.called("sudo")[0], ["sudo", "-n", "true"])  # Before the first command that needs root.
        # Each block alone, in a shell where the one before it has not passed: refused, nothing done.
        shutil.rmtree(self.base)
        for name, needs in (("B", "A"), ("C", "B"), ("D", "C")):
            with self.subTest(block=name):
                out = self.run_blocks(name, before=f"run={RUN}\ngate=\n")
                self.assertIn(f"STOP: block {needs} has not passed in this shell", out)
                self.assertIn(f"gate= ({name} expected)", out)
                self.assertFalse(self.base.exists())
                self.assertEqual(self.called("sudo") + self.called("docker", "load") + self.called("flock"), [])

    def test_every_failed_check_of_block_a_stops_all_later_blocks_of_one_paste(self):
        root = str(self.docker_root)
        cases = {
            "the Docker context is not default": {"context": "remote"},
            "Docker is not on a local Unix socket": {"endpoint": "tcp://10.0.0.5:2376"},
            "Docker's data root is unknown": {"docker_root": ""},
            "the image tag komika-postgres:18.6-pgbackrest-2.59 exists on this host already": {"image": True},
            "a container, volume or network carries a cc.komiq.rehearsal label already": {"labelled": ["abc123"]},
            "these names exist already:\nkomika-pg-rehearsal-" + RUN + "-postgres": {
                "containers": ["komika-server-1", f"komika-pg-rehearsal-{RUN}-postgres"]},
            "these names exist already:\nkomika-pitr-1234": {"volumes": ["komika-pitr-1234"]},
            "a network uses a subnet of the rehearsal already:\nkomika_komika 172.30.21.0/24": {"subnet": "172.30.21.0/24"},
            "a network uses a subnet of the rehearsal already:\nkomika_komika 172.29.42.0/24": {"subnet": "172.29.42.0/24"},
            "a container mounts a directory that holds Docker's volumes or the rehearsal's directory:\n/\n": {"sources": ["/", "/etc/komika"]},
            "a container mounts a directory that holds Docker's volumes or the rehearsal's directory:\n" + str(self.srv) + "\n": {"sources": [str(self.srv)]},
            "a container mounts a directory that holds Docker's volumes or the rehearsal's directory:\n" + root + "\n": {"sources": [root]},
            "a container mounts a directory that holds Docker's volumes or the rehearsal's directory:\n" + root + "/volumes\n": {"sources": [root + "/volumes"]},
            "a container mounts a directory that holds Docker's volumes or the rehearsal's directory:\n" + str(self.top) + "\n": {"sources": [str(self.top)]},
            f"{self.lock_directory} is not a root-owned sticky directory": {"lock_directory_state": "0 drwxr-xr-x"},
            f"{self.lock_directory} is not a root-owned sticky directory (drwxrwxrwt)": {"lock_directory_state": "1000 drwxrwxrwt"},
        }
        saved = dict(self.state)
        selected = "DOCKER_HOST or DOCKER_CONTEXT is set in this shell"
        cases[selected] = {}
        cases[selected + "; "] = {}
        for text, change in cases.items():
            with self.subTest(text=text[:60]):
                self.state = dict(saved, **change)
                # G7: a daemon chosen by the environment is refused although its socket is a local one.
                extra = {} if selected not in text else (
                    {"DOCKER_HOST": "unix:///run/other.sock"} if text == selected else {"DOCKER_CONTEXT": "default"})
                out = self.run_blocks("A", "B", "C", "D", **extra)  # All four pasted at once.
                self.assertIn("STOP: " + text, out)
                self.assertNotIn("BLOCK A OK", out)
                for letter in "ABCD":
                    self.assertIn(f"gate= ({letter} expected)", out)
                self.assertIn("STOP: block A has not passed in this shell", out)
                self.assertFalse(self.base.exists())
                self.assertEqual(self.called("sudo") + self.called("docker", "load") + self.called("flock")
                                 + self.called("install"), [])
        # What the kit itself accepts is no STOP here: another volume's own directory, a socket.
        self.state = dict(saved, sources=[root + "/volumes/komika_server-data/_data", "/var/run/docker.sock", root + "/containers"])
        self.assertIn("BLOCK A OK", self.run_blocks("A"))

    def test_block_b_never_touches_a_directory_that_exists_already(self):
        """F2: `sudo install -d` runs only after its own guard has passed."""
        for make in (lambda: self.base.mkdir(), lambda: self.base.symlink_to(self.top), lambda: self.base.write_text("")):
            make()
            with self.subTest(kind=os.lstat(self.base).st_mode):
                before = os.lstat(self.base)
                out = self.run_blocks("A", "B", "C", "D")
                self.assertIn("BLOCK A OK", out)
                self.assertIn(f"STOP: {self.base} exists already, or sudo needs a password", out)
                self.assertIn("no existing directory was changed", out)
                self.assertIn("gate=A (B expected)", out)
                self.assertIn("STOP: block B has not passed in this shell", out)
                self.assertEqual(self.called("sudo") + self.called("install") + self.called("docker", "load"), [])
                after = os.lstat(self.base)
                self.assertEqual((before.st_mode, before.st_uid, before.st_ino), (after.st_mode, after.st_uid, after.st_ino))
                if self.base.is_dir() and not self.base.is_symlink():
                    self.assertEqual(os.listdir(self.base), [])
            if self.base.is_dir() and not self.base.is_symlink():
                self.base.rmdir()
            else:
                self.base.unlink()

    def test_block_b_stops_at_a_password_prompt_a_half_made_directory_and_an_empty_baseline(self):
        """G5, G7, G2: no prompt can swallow a pasted line; each STOP text is true; no empty baseline."""
        found = re.findall(r"^```sh\n(.*?)^```", (KIT / "RESTORE.md").read_text(), flags=re.S | re.M)
        # The sudo line is pasted alone, before the block that needs it. It asks only when a
        # password is needed: a bare `sudo -v` prompts on a host with a password-free rule
        # beside a rule with a password (found on the test host and on the production host).
        for start in ("# B. ", 'target="$root/pitr"'):
            index = [n for n, body in enumerate(found) if body.startswith(start)]
            self.assertEqual(found[index[0] - 1], "sudo -n true || sudo -v\n", start)
        saved = dict(self.state)
        # sudo would ask for a password: the block stops before `sudo install`, nothing exists afterwards.
        self.state = dict(saved, sudo=False)
        out = self.run_blocks("A", "B", "C", "D")
        self.assertIn(f"STOP: {self.base} exists already, or sudo needs a password (run 'sudo -v' alone first)", out)
        self.assertIn("gate=A (B expected)", out)
        self.assertEqual(self.called("sudo"), [["sudo", "-n", "true"]])
        self.assertEqual(self.called("install") + self.called("docker", "load") + self.called("flock"), [])
        self.assertFalse(os.path.lexists(self.base))
        # The directory was made, a subdirectory was not: the text says exactly that, and nothing goes on.
        self.state = dict(saved)
        (self.bin / "mkdir").write_text(STAND_IN)
        (self.bin / "mkdir").chmod(0o755)
        out = self.run_blocks("A", "B", "C", "D")
        (self.bin / "mkdir").unlink()
        self.assertIn(f"STOP: {self.base} was created by this block, but not all of its subdirectories", out)
        self.assertNotIn("no existing directory was changed", out)
        self.assertIn("gate=A (B expected)", out)
        self.assertEqual(os.listdir(self.base), [])
        self.assertEqual(self.called("docker", "load") + self.called("flock"), [])
        self.base.rmdir()
        # An empty baseline would make every later comparison pass without comparing anything.
        for name, change in (("no other container", {"containers": []}),
                             ("a missing label is not printed as an empty string", {"no_label": "<no value>"})):
            with self.subTest(case=name):
                self.state = dict(saved, **change)
                out = self.run_blocks("A", "B", "C", "D")
                self.assertIn("BLOCK A OK", out, self.output)
                self.assertIn("STOP: the baseline could not be written or is empty", out)
                self.assertNotIn("BLOCK B OK", out)
                self.assertIn("gate=A (B expected)", out)
                self.assertIn("STOP: block B has not passed in this shell", out)
                self.assertEqual(self.called("docker", "load") + self.called("flock"), [])
                shutil.rmtree(self.base)
        # `after` refuses to compare against a baseline that is missing or empty.
        self.state = dict(saved)
        for name, damage in (("missing", 'rm "$records/others-before.txt"'), ("empty", ': > "$records/others-before.txt"')):
            with self.subTest(baseline=name):
                out = self.run_blocks("A", "B", after=damage + '\nafter probe; echo "after status=$?"\n')
                self.assertIn("BLOCK B OK", out, self.output)
                self.assertIn(f"STOP after probe: the baseline {self.base}/records/others-before.txt is missing or empty", out)
                self.assertNotIn("OK after probe", out)
                self.assertIn("after status=1", out)
                shutil.rmtree(self.base)
        out = self.run_blocks("A", "B", after='after probe; echo "after status=$?"\n')
        self.assertIn("OK after probe: no container outside the rehearsal changed\nafter status=0", out, self.output)

    def test_block_c_stops_before_the_load_and_block_d_without_the_lease(self):
        self.stage()
        cases = {
            "a transferred file does not have its recorded hash": 'printf x >> "$base/transfer/komika-server"\n',
            "the env file, the SQLite copy or the binary is missing": 'rm "$sqlite"\n',
            "the env file is not your own regular file with mode 600": 'chmod 644 "$env_file"\n',
            "the env file is not your own regular file with mode 600\n": 'mv "$env_file" "$env_file.real" && ln -s "$env_file.real" "$env_file"\n',
            "the SQLite copy is not a standalone regular file": 'printf x > "$sqlite-wal"\n',
            "the SQLite copy is not a standalone regular file\n": 'printf x > "$sqlite-journal"\n',
            "the migration history of the SQLite copy could not be read": 'printf x > "$sqlite"\n',
        }
        for text, damage in cases.items():
            with self.subTest(text=text[:50]):
                out = self.run_blocks("A", "B", after=self.provision() + damage + self.blocks["C"] + self.blocks["D"])
                self.assertIn("STOP: " + text, out, self.output)
                self.assertIn("gate=B (C expected)", out)
                self.assertIn("STOP: block C has not passed in this shell", out)
                self.assertEqual(self.called("docker", "load") + self.called("flock"), [])
                self.assertFalse((self.base / "kit" / "rehearsal.sh").exists())
                self.assertTrue((self.base / "transfer" / "komika-postgres-image.tar.gz").exists())
                shutil.rmtree(self.base)
        # G7: `gzip -l` prints nothing, or a size of zero: the space check must not go on without the image.
        for name, fake in (("nothing", 'gzip() { test "$1" = -l || command gzip "$@"; }\n'),
                           ("zero", 'gzip() { if test "$1" = -l; then printf "compressed uncompressed ratio name\\n 40 0 0.0%% x\\n"; '
                                    'else command gzip "$@"; fi; }\n')):
            with self.subTest(gzip=name):
                out = self.run_blocks("A", "B", after=self.provision() + fake + self.blocks["C"] + self.blocks["D"])
                self.assertIn("STOP: image size unknown; nothing was loaded", out, self.output)
                self.assertNotIn("headroom ok", out)
                self.assertIn("gate=B (C expected)", out)
                self.assertEqual(self.called("docker", "load") + self.called("flock"), [])
                self.assertFalse((self.base / "kit" / "rehearsal.sh").exists())
                shutil.rmtree(self.base)
        # Too little space for the image, 2.7 x the SQLite file and the floor: nothing is loaded.
        need = 3 * 1024 ** 3 + 12000 + 8192 * 27 // 10  # 8192: the size of the staged SQLite copy
        self.state["free"] = str(need - 1)
        out = self.run_blocks("A", "B", after=self.provision() + self.blocks["C"])
        self.assertIn(f"STOP: image, import peak and floor need {need} bytes on Docker's data root, "
                      f"{need - 1} are free; nothing was loaded", out)
        self.assertEqual(self.called("docker", "load"), [])
        shutil.rmtree(self.base)
        self.state["free"] = str(need)
        self.assertIn("BLOCK C OK", self.run_blocks("A", "B", after=self.provision() + self.blocks["C"]), self.output)
        shutil.rmtree(self.base)
        self.state["free"] = "80000000000"
        # A container outside the rehearsal changed during the load.
        out = self.run_blocks("A", "B", after=self.provision() + "sed -i.bak 's/2026-10-01/2026-10-02/' "
                              + str(self.bin / "state.json") + "\n" + self.blocks["C"] + self.blocks["D"])
        self.assertIn("STOP after load: a container outside the rehearsal changed", out)
        self.assertIn("gate=B (C expected)", out)
        self.assertEqual(self.called("flock"), [])
        shutil.rmtree(self.base)
        # G4: what the runbook says about that STOP. The image is loaded and the tarball gone; block C
        # a second time fails at its first check and loads nothing; `gate=C` by hand lets block D run.
        churn = "sed -i.bak 's/2026-10-01/2026-10-02/' " + str(self.bin / "state.json") + "\n"
        out = self.run_blocks("A", "B", after=self.provision() + churn + self.blocks["C"] + self.blocks["C"]
                              + "gate=C\n" + self.blocks["D"])
        self.assertEqual(out.count("STOP after load: a container outside the rehearsal changed"), 1)
        self.assertEqual(out.count("STOP: a transferred file does not have its recorded hash"), 1)
        self.assertEqual(len(self.called("docker", "load")), 1)
        self.assertFalse((self.base / "transfer" / "komika-postgres-image.tar.gz").exists())
        self.assertIn("BLOCK D OK", out, self.output)
        self.assertIn("gate=D (D expected)", out)
        shutil.rmtree(self.base)
        # Block D: the lease is taken, or a foreign variable is exported.
        self.state["lease"] = False
        out = self.run_blocks("A", "B", after=self.provision() + self.blocks["C"] + self.blocks["D"])
        self.assertIn("STOP: another operator session holds the lease", out)
        self.assertIn("gate=C (D expected)", out)
        shutil.rmtree(self.base)
        self.state["lease"] = True
        # G7: the lease file is opened for appending and never through a symbolic link.
        self.lease.unlink()
        victim = self.top / "somebody-elses-file"
        victim.write_text("kept bytes")
        self.lease.symlink_to(victim)
        out = self.run_blocks("A", "B", after=self.provision() + self.blocks["C"] + self.blocks["D"])
        self.assertIn("STOP: the operator lease file is a symbolic link or could not be opened", out, self.output)
        self.assertIn("gate=C (D expected)", out)
        self.assertEqual(self.called("flock"), [])
        self.assertEqual(victim.read_text(), "kept bytes")
        shutil.rmtree(self.base)
        self.lease.unlink()
        self.lease.write_text("kept bytes")
        out = self.run_blocks("A", "B", after=self.provision() + self.blocks["C"] + self.blocks["D"])
        self.assertIn("BLOCK D OK", out, self.output)
        self.assertEqual(self.lease.read_text(), "kept bytes")
        shutil.rmtree(self.base)
        out = self.run_blocks("A", "B", after=self.provision() + self.blocks["C"] + self.blocks["D"],
                              KOMIKA_R2_PREFIX="komika")
        self.assertIn("STOP: the shell exports another KOMIKA_, DOCKER_, COMPOSE_ or PGBACKREST_ variable", out)
        self.assertIn("gate=C (D expected)", out)

    def names(self):
        return textwrap.dedent(f"""
            run={RUN}
            base={self.base}
            root=$base/root
            records=$base/records
            docker_root={self.docker_root}
            export KOMIKA_REHEARSAL_PROJECT=komika-pg-rehearsal-$run KOMIKA_REHEARSAL_ROOT=$root
            after() {{ echo "after $1"; }}
        """)

    def restored(self):
        target = self.base / "root" / "pitr"
        target.mkdir(parents=True)
        (self.base / "root" / ".komika-rehearsal-root").write_text("marker")
        (target / "PG_VERSION").write_text("18\n")
        (target / "base").mkdir()
        (self.base / "records").mkdir(exist_ok=True)
        return target

    def test_each_rm_runs_only_as_the_last_link_of_its_identity_check(self):
        """F1: no path of the two removal blocks removes anything unless every test passed."""
        victim = self.top / "production"
        victim.mkdir()
        (victim / "PG_VERSION").write_text("18\n")
        (victim / "data").write_text("production bytes")
        target = self.restored()
        out = self.run_blocks("pitr", before=self.names())
        self.assertIn(f"IDENTITY OK: {target}\nREMOVED: {target}\nafter remove-pitr", out, self.output)
        self.assertFalse(target.exists())
        self.assertTrue((self.base / "root" / ".komika-rehearsal-root").exists())

        def link():
            shutil.rmtree(target)
            target.symlink_to(victim)

        def moved_root():
            return "root=" + str(victim.parent) + "\n"

        failing = {
            "another root in the variable": (None, "root=" + str(self.top / "elsewhere") + "\n"),
            "the exported root differs": (None, "export KOMIKA_REHEARSAL_ROOT=" + str(self.top) + "\n"),
            "the exported root is unset": (None, "unset KOMIKA_REHEARSAL_ROOT\n"),
            "a link to production": (link, ""),
            "no marker beside it": (lambda: (self.base / "root" / ".komika-rehearsal-root").unlink(), ""),
            "not PostgreSQL 18": (lambda: (target / "PG_VERSION").write_text("17\n"), ""),
            "no data directory": (lambda: (target / "PG_VERSION").unlink(), ""),
            "something is mounted below it": (lambda: self.state.update(mounted=["/", str(target) + "/base"]), ""),
            "a restore container still uses it": (lambda: self.state.update(labelled=["abc123"]), ""),
            # G1: a command that fails prints nothing; nothing is not "no mount" and not "no container".
            "findmnt fails": (lambda: self.state.update(findmnt_fails=True), ""),
            "findmnt prints nothing": (lambda: self.state.update(mounted=[]), ""),
            "docker fails": (lambda: self.state.update(docker_fails=True), ""),
            # G5: a password prompt would read the next pasted line.
            "sudo would ask for a password": (lambda: self.state.update(sudo=False), ""),
        }
        clean = {"mounted": ["/", "/srv-other"], "labelled": [], "findmnt_fails": False, "docker_fails": False, "sudo": True}
        for name, (damage, shell) in failing.items():
            with self.subTest(case=name):
                if self.base.exists():
                    shutil.rmtree(self.base)
                self.state.update(clean)
                target = self.restored()
                if damage:
                    damage()
                out = self.run_blocks("pitr", before=self.names() + shell)
                self.assertNotIn("IDENTITY OK", out)
                self.assertNotIn("REMOVED", out)
                self.assertEqual(self.called("rm"), [], name)
                if name == "sudo would ask for a password":  # Stopped before the first command that could prompt.
                    self.assertEqual(self.called("sudo"), [["sudo", "-n", "true"]])
                self.assertTrue(os.path.lexists(target))
                self.assertEqual((victim / "data").read_text(), "production bytes")
        # The whole directory of the run, at the very end.
        self.state.update(clean)
        shutil.rmtree(self.base)
        self.restored()
        out = self.run_blocks("base", before=self.names())  # The restored directory is still there.
        self.assertNotIn("REMOVED", out)
        self.assertEqual(self.called("rm"), [])
        shutil.rmtree(self.base / "root" / "pitr")
        for name, change, shell in (("a container of the project is left", {"labelled": ["abc123"]}, ""),
                                    ("something is mounted in it", {"mounted": ["/", str(self.base) + "/root"]}, ""),
                                    ("another base in the variable", {}, "base=" + str(victim) + "\n"),
                                    ("findmnt fails", {"findmnt_fails": True}, ""),
                                    ("findmnt prints nothing", {"mounted": []}, ""),
                                    ("docker fails", {"docker_fails": True}, ""),
                                    ("the project is not selected", {}, "unset KOMIKA_REHEARSAL_PROJECT\n"),
                                    ("another project is selected", {}, "KOMIKA_REHEARSAL_PROJECT=komika-pg-rehearsal-other\n"),
                                    ("the project is empty", {}, "KOMIKA_REHEARSAL_PROJECT=\n"),
                                    # The directory is an entry of root's /srv: the removal needs sudo.
                                    ("sudo would ask for a password", {"sudo": False}, "")):
            with self.subTest(case=name):
                self.state.update(clean)
                self.state.update(change)
                out = self.run_blocks("base", before=self.names() + shell)
                self.assertNotIn("IDENTITY OK", out)
                self.assertNotIn("REMOVED", out)
                self.assertEqual(self.called("rm"), [])
                self.assertTrue(self.base.exists() and victim.exists())
        self.state.update(clean)
        out = self.run_blocks("base", before=self.names())
        self.assertIn(f"IDENTITY OK: {self.base}\nREMOVED: {self.base}\n", out, self.output)
        self.assertFalse(self.base.exists())
        self.assertEqual((victim / "data").read_text(), "production bytes")

    def test_the_text_beside_the_blocks_says_what_the_kit_prints_and_what_must_not_be_pasted(self):
        """G3, G6, G7: sentences an operator acts on; each is tied to the code or to a block."""
        text = (KIT / "RESTORE.md").read_text()
        flat = " ".join(text.split())
        code = (KIT / "rehearsal.py").read_text()
        # G3: the words of `status` that decide whether SIGTERM may be sent are the words the kit prints.
        for words in ("process=running", "process=NOT running (it ended without cleanup; the lock itself is free)",
                      "process=running as another user", "operation recorded: none"):
            self.assertIn(words, flat)
            self.assertIn(words.replace("process=", ""), code, words)
        self.assertIn('f"process={alive}")', code)
        self.assertIn("only if that line ends in `process=running` for that pid and `ps -o args= -p N` shows `rehearsal`", flat)
        # G7: the drill lines do not stop themselves.
        drill = text.index('test "$gate" = D && echo "pre-flight passed')
        self.assertIn("Never paste this block whole", text[text.index("### Full drill order"):drill])
        self.assertIn("one line at a time", text[text.index("### Full drill order"):drill])
        # G4: the one dead end of block C has its way out written down.
        self.assertIn("If `STOP after load` is the only `STOP` of block C", flat)
        self.assertIn("Never paste block C a second time", flat)
        # G6: the evidence is copied off the host before the block that removes it; no secret directory is named.
        found = re.findall(r"^```sh\n(.*?)^```", text, flags=re.S | re.M)
        last = [n for n, body in enumerate(found) if body.startswith("cd / \\\n")][0]
        evidence = found[last - 1]
        self.assertIn(f'scp -r -- HOST:/srv/komika-pg-{RUN}/records "$out/evidence/"', evidence)
        self.assertIn(f"scp -- 'HOST:/srv/komika-pg-{RUN}/root/*.json' \"$out/evidence/\"", evidence)
        self.assertNotIn("private", evidence)
        self.assertNotIn("input", evidence)
        self.assertNotIn("rm ", evidence)


if __name__ == "__main__":
    unittest.main()
