"""Run the rehearsal entry point against a daemon-free fake `docker` and check
every command line it builds. No argument of the caller may ever reach Docker.

Tests marked shell=True execute the real rehearsal.sh; the many refusal
combinations call the same rehearsal.main() in this process to stay fast.
"""
import json
import os
from pathlib import Path
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch
import drill
import rehearsal
import safety
import test_support as t

SLEEP = time.sleep  # The real one, for tests that replace time.sleep while the kit waits.

FAKE_DOCKER = r'''#!/usr/bin/env python3
import hashlib, json, os, sys, time
args = sys.argv[1:]
# rehearsal.sh passes almost no environment on: state lives beside this file.
here = os.path.dirname(os.path.abspath(sys.argv[0]))
state = json.load(open(os.path.join(here, "state.json")))
stdin = sys.stdin.read() if args[:1] in (["run"], ["create"]) and "/dev/stdin" in args else ""
pairs = dict(line.split("=", 1) for line in stdin.splitlines() if "=" in line)
with open(os.path.join(here, "calls.jsonl"), "a") as out:
    out.write(json.dumps({
        "args": args, "docker_host": os.environ.get("DOCKER_HOST"),
        "environment": sorted(os.environ),
        "stdin_keys": sorted(pairs),
        # Values of settings that are no credential, to prove WHICH prefix arrives.
        "stdin_values": {key: value for key, value in pairs.items()
                         if key in ("KOMIKA_R2_PREFIX", "KOMIKA_PRODUCTION_R2_PREFIX", "KOMIKA_REHEARSAL_PROJECT",
                                    "KOMIKA_R2_VERIFY_TLS", "KOMIKA_R2_BUCKET", "KOMIKA_R2_ENDPOINT",
                                    "KOMIKA_PG_ALLOWED_CIDRS")},
        "stdin_has_password": state["password"] in stdin}) + "\n")

def save():
    with open(os.path.join(here, "state.json"), "w") as out:
        json.dump(state, out)

def option(name):
    return [args[index + 1] for index, arg in enumerate(args[:-1]) if arg == name]

def created(running):
    labels = dict(item.split("=", 1) for item in option("--label"))
    mounts = [dict(part.split("=", 1) for part in item.split(",") if "=" in part) for item in option("--mount")]
    name = option("--name")[0]
    return {"Id": hashlib.sha256(name.encode()).hexdigest(), "Name": "/" + name, "Image": "sha256:fake",
            "State": {"Status": "running" if running else "created", "Running": running, "StartedAt": "now"},
            "Config": {"Labels": labels},
            "Mounts": [{"Type": m.get("type"), "Source": m.get("src"), "Destination": m.get("dst")} for m in mounts]}

def labels(entry):
    return entry.get("Config", {}).get("Labels") or entry.get("Labels") or {}

def filtered(entries):
    result = []
    for entry in entries:
        keep = True
        for index, arg in enumerate(args):
            if arg == "--filter":
                key, _, value = args[index + 1][len("label="):].partition("=")
                keep = keep and key in labels(entry) and (not value or labels(entry)[key] == value)
        if keep:
            result.append(entry)
    return result

def inspect(entries, wanted):
    found = [e for e in entries if e.get("Id") in wanted or str(e.get("Name")).lstrip("/") in wanted]
    if len(found) != len(wanted):
        sys.exit(1)
    print(json.dumps(found))

if args[:2] == ["context", "show"]:
    print(state["context"])
elif args[:2] == ["context", "inspect"]:
    print(json.dumps([{"Endpoints": {"docker": {"Host": state["endpoint"]}}}]))
elif args[0] == "compose":
    if "config" in args:
        print(json.dumps(state["config"]))
    elif "version" in args:
        print("fake")
    elif "up" in args:
        # What `compose up` creates in this state (volumes, containers), if anything.
        for kind, entries in state["up_creates"].items():
            state[kind] += entries
        save()
elif args[:2] == ["image", "inspect"]:
    if not state["image"]:
        sys.exit(1)
    print("sha256:fake")
elif args[0] == "info":
    facts = {"DockerRootDir": state["docker_root"], "MemTotal": 1, "NCPU": 1, "ServerVersion": "fake", "OperatingSystem": "fake"}
    print(json.dumps(facts["DockerRootDir"] if "DockerRootDir" in args[-1] else facts))
elif args[0] == "ps":
    print("\n".join(e["Name"].lstrip("/") if "--format" in args else e["Id"] for e in filtered(state["containers"])))
elif args[:2] == ["container", "inspect"]:
    inspect(state["containers"], args[2:])
elif args[:2] == ["volume", "ls"]:
    print("\n".join(e["Name"] for e in filtered(state["volumes"])))
elif args[:2] == ["volume", "inspect"]:
    inspect(state["volumes"], args[2:])
elif args[:2] == ["network", "ls"]:
    print("\n".join(e["Name"] if "--format" in args else e["Id"] for e in filtered(state["networks"])))
elif args[:2] == ["network", "inspect"]:
    inspect(state["networks"], args[2:])
elif args[:2] == ["network", "create"]:
    labels = dict(item.split("=", 1) for item in option("--label"))
    state["networks"].append({"Id": hashlib.sha256(args[-1].encode()).hexdigest(), "Name": args[-1], "Labels": labels,
                              "IPAM": {"Config": [{"Subnet": option("--subnet")[0]}]}})
    save()
elif args[:2] == ["network", "rm"]:
    state["networks"] = [e for e in state["networks"] if args[2] not in (e["Id"], e["Name"])]
    save()
elif args[0] == "create":
    state["containers"].append(created(False))
    save()
elif args[0] == "rm":
    state["containers"] = [e for e in state["containers"] if args[-1] not in (e["Id"], e["Name"].lstrip("/"))]
    save()
elif args[0] == "exec":
    if state.get("exec_fails") and state["exec_fails"] in " ".join(args):
        sys.exit(1)
    for needle, output in state["exec"].items():
        if needle in " ".join(args):
            print(output)
    for needle, status in state["exec_status"].items():
        if needle in " ".join(args):
            sys.exit(status)
elif args[0] == "run":
    if "sha256sum" in args:  # The image identity check: what the image carries.
        for path in args[args.index("sha256sum") + 2:]:
            name = os.path.basename(path)
            content = open(os.path.join(state["kit"], name), "rb").read() + state["image_differs"].get(name, "").encode()
            if name not in state["image_lacks"]:
                print(hashlib.sha256(content).hexdigest() + "  " + path)
        sys.exit(1 if state["image_lacks"] else 0)
    if state["run_late"] and "-job-" in " ".join(option("--name")):
        # The daemon creates the container only after the client was killed.
        entry = created(True)
        if os.fork() == 0:
            os.setsid()
            # The daemon is no child of the client: it holds none of the client's streams.
            null = os.open(os.devnull, os.O_RDWR)
            for descriptor in (0, 1, 2):
                os.dup2(null, descriptor)
            if state["run_late"] == "gate":
                # Not after a delay that a slow host could outrun: when the test opens the gate.
                gate, limit = os.path.join(here, "late-gate"), time.time() + 900
                while not os.path.exists(gate):
                    if not os.path.isdir(here) or time.time() > limit:
                        os._exit(0)
                    time.sleep(0.02)
            else:
                time.sleep(state["run_late"])
            with open(os.path.join(here, "state.json")) as source:
                state = json.load(source)
            state["containers"].append(entry)
            save()
            os._exit(0)
        time.sleep(60)
    if state["run_lingers"] and "-job-" in " ".join(option("--name")):
        # An interrupted client: the container exists and keeps running.
        state["containers"].append(created(True))
        save()
        time.sleep(state["run_lingers"])
    for arg in args:
        if arg.startswith("type=bind,src=") and arg.endswith(",dst=/work"):
            with open(arg[len("type=bind,src="):-len(",dst=/work")] + "/report.json", "w") as report:
                report.write('{"complete": true}')
    for needle, output in state["run"].items():
        if needle in args:
            print(output)
            break
    else:
        print(state["run_output"])
    sys.exit(state["run_status"])
'''
READ_ONLY = (["context"], ["info"], ["ps"], ["container", "inspect"], ["volume", "ls"], ["volume", "inspect"],
             ["network", "ls"], ["network", "inspect"], ["image", "inspect"])
PRODUCTION_VOLUME = "/var/lib/docker/volumes/komika_server-data/_data"
# A host mount table for the injected Linux platform: one filesystem, no alias.
MOUNTINFO = "25 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw\n"
CONTROL = """max_connections setting:              48
max_worker_processes setting:         8
max_wal_senders setting:              10
max_prepared_xacts setting:           0
max_locks_per_xact setting:           64
"""


def identity_call(call):
    """The one fixed command line that reads the kit files' hashes from the image."""
    return call[:1] == ["run"] and "sha256sum" in call


def volume(name, labelled=True, mountpoint=None):
    """The `docker volume inspect` shape of a plain local volume."""
    return {"Name": name, "Driver": "local", "Options": None, "Scope": "local",
            "Labels": {t.LABEL: t.PROJECT} if labelled else {},
            "Mountpoint": mountpoint or f"{t.DOCKER_ROOT}/volumes/{name}/_data"}


def network(name, subnet, labelled=True, **labels):
    return {"Id": ("f" * 60 + "%04d" % (len(name))), "Name": name, "Labels": dict({t.LABEL: t.PROJECT} if labelled else {}, **labels),
            "IPAM": {"Config": [{"Subnet": subnet}]}}


class Harness(unittest.TestCase):
    kit = safety.KIT

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name).resolve()
        (self.base / "bin").mkdir()
        binary = self.base / "bin" / "docker"
        # The running interpreter directly: no launcher shim, a much faster suite.
        binary.write_text(FAKE_DOCKER.replace("#!/usr/bin/env python3", "#!" + sys.executable, 1))
        binary.chmod(0o700)
        self.socket_path = self.base / "s"
        listener = socket.socket(socket.AF_UNIX)
        listener.bind(str(self.socket_path))
        self.addCleanup(listener.close)
        self.env_file = self.base / "rehearsal.env"
        self.write_env(t.VALUES)
        self.root = self.base / "root"
        self.root.mkdir(mode=0o700)
        (self.root / safety.ROOT_MARKER).write_text(safety.marker_text(os.getuid()))
        (self.base / "docker-data").mkdir()
        # The fixtures live below this test's own Docker data root, and the
        # operation lock is this test's own file: both are module values that
        # tests can replace, a caller's environment or arguments cannot.
        self.lock_path = str(self.base / "komika-pg-rehearsal.lock")
        self.shell_kit = None
        # True while another process of the test owns the fake daemon's state file:
        # a run then reads what is there instead of writing self.state over it.
        self.live_state = False
        for module, name, value in ((t, "DOCKER_ROOT", str(self.base / "docker-data")),
                                    (safety, "LOCK_PATH", self.lock_path)):
            patcher = patch.object(module, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)
        self.log = self.base / "bin" / "calls.jsonl"
        self.state_file = self.base / "bin" / "state.json"
        self.state = {
            "context": "default", "endpoint": "unix://" + str(self.socket_path), "config": t.rendered(),
            "image": True, "docker_root": str(self.base / "docker-data"), "password": t.PASSWORD,
            "containers": [t.container()],
            "volumes": [volume(t.PROJECT + "-postgres-data"), volume(t.PROJECT + "-postgres-socket"),
                        volume("komika_server-data", labelled=False, mountpoint=str(self.base / "docker-data" / "volumes" / "komika_server-data"))],
            "networks": [network(t.PROJECT + "-network", "172.30.21.0/24"), network("komika_komika", "172.21.0.0/16", labelled=False)],
            "exec": {}, "exec_fails": "", "exec_status": {}, "run": {}, "run_output": "", "run_status": 0, "run_lingers": 0,
            "run_late": 0, "up_creates": {},
            "kit": str(safety.KIT), "image_differs": {}, "image_lacks": [],
        }
        self.env = {"PATH": str(self.base / "bin") + os.pathsep + os.environ["PATH"], "HOME": str(self.base),
                    "KOMIKA_REHEARSAL_PROJECT": t.PROJECT, "KOMIKA_COMPOSE_ENV_FILE": str(self.env_file),
                    "KOMIKA_REHEARSAL_ROOT": str(self.root), "CALLER_CANARY": "must not reach docker"}

    def write_env(self, values, mode=0o600):
        self.env_file.write_text("".join(f"{key}={value}\n" for key, value in values.items()))
        self.env_file.chmod(mode)

    def rehome(self, docker_root):
        """Another data root of the daemon: the rehearsal's own volumes and mounts are below it."""
        old, new = self.state["docker_root"], str(docker_root)
        text = json.dumps(self.state).replace(json.dumps(f"{old}/volumes/{t.PROJECT}")[1:-1],
                                              json.dumps(f"{new}/volumes/{t.PROJECT}")[1:-1])
        self.state = json.loads(text)
        self.state["docker_root"] = new
        t.DOCKER_ROOT = new  # Patched in setUp; its cleanup restores the module value.

    def absent_project(self):
        """Nothing of the rehearsal project exists yet (what `preflight --init-root` requires)."""
        self.state["containers"] = []
        self.state["volumes"] = [e for e in self.state["volumes"] if not e["Name"].startswith(t.PROJECT)]
        self.state["networks"] = [e for e in self.state["networks"] if not e["Name"].startswith(t.PROJECT)]

    def no_repository(self):
        self.write_env(t.NO_REPO_VALUES)
        self.state["config"] = t.rendered(no_repo=True)
        self.state["containers"] = [t.container(no_repo=True)]

    def linux(self):
        """Run in-process calls as on the Linux target, on every development platform.

        The platform and the mount table are module values of safety.py: tests
        can replace them, a caller's environment or arguments cannot.
        """
        for name, value in (("PLATFORM", "linux"), ("read_mountinfo", lambda: MOUNTINFO)):
            patcher = patch.object(safety, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)

    def free(self, docker=None, root=None, same=True):
        """Replace the free-space reading: bytes per filesystem, one device or two."""
        if not hasattr(self, "space"):
            real = safety.free_bytes

            def reading(path):
                inside = str(path).startswith(self.state["docker_root"])
                value = self.space["docker" if inside else "root"]
                return (real(path)[0] if value is None else value, 1 if self.space["same"] or inside else 2)

            patcher = patch.object(safety, "free_bytes", side_effect=reading)
            patcher.start()
            self.addCleanup(patcher.stop)
        self.space = {"docker": docker, "root": root, "same": same}

    def in_process(self, args, env):
        """rehearsal.main() with the same environment; output captured at fd level."""
        with tempfile.TemporaryFile("w+") as out, tempfile.TemporaryFile("w+") as err, \
                patch.dict(os.environ, env, clear=True):
            sys.stdout.flush()
            sys.stderr.flush()
            saved = os.dup(1), os.dup(2)
            os.dup2(out.fileno(), 1)
            os.dup2(err.fileno(), 2)
            try:
                status = rehearsal.main(list(args))
            finally:
                sys.stdout.flush()
                sys.stderr.flush()
                os.dup2(saved[0], 1)
                os.dup2(saved[1], 2)
                os.close(saved[0])
                os.close(saved[1])
                drill.REHEARSAL = None
            out.seek(0)
            err.seek(0)
            return subprocess.CompletedProcess(args, status, out.read(), err.read())

    def private(self, kit, **constants):
        """Give a COPY of the kit this test's lock path (and timing constants).

        Nothing in the environment or on a command line can set them, so a
        real process needs a copy whose safety.py carries the test's values.
        """
        self.assertNotEqual(kit, safety.KIT)
        path = kit / "safety.py"
        text = path.read_text()
        for name, value in dict(LOCK_PATH=self.lock_path, **constants).items():
            text, count = re.subn(rf"^{name} = .*$", f"{name} = {value!r}", text, flags=re.M)
            self.assertEqual(count, 1, name)
        path.write_text(text)
        return kit

    def process_kit(self, **constants):
        """The kit that a real `rehearsal.sh` process of this test runs."""
        if self.kit != safety.KIT:
            return self.private(self.kit, **constants)
        if self.shell_kit is None:
            self.shell_kit = self.base / "kit-process"
            shutil.copytree(safety.KIT, self.shell_kit, ignore=shutil.ignore_patterns("__pycache__", "test_*"))
        return self.private(self.shell_kit, **constants)

    def prepare_process(self, **constants):
        """State and log for a process that the test starts itself (Popen)."""
        kit = self.process_kit(**constants)
        self.state_file.write_text(json.dumps(dict(self.state, kit=str(kit))))
        if self.log.exists():
            self.log.unlink()
        return kit

    def run_rehearsal(self, *args, script="rehearsal.sh", shell=False, **env):
        process = shell or script != "rehearsal.sh" or self.kit != safety.KIT
        # The legacy entry points take no lock: they run from the kit itself.
        self.ran = self.process_kit() if process and script == "rehearsal.sh" else self.kit
        self.state["kit"] = str(self.ran)
        if not self.live_state:
            self.state_file.write_text(json.dumps(self.state))
        if self.log.exists():
            self.log.unlink()
        if process:
            result = subprocess.run(["sh", str(self.ran / script), *args], env=dict(self.env, **env),
                                    capture_output=True, text=True, stdin=subprocess.DEVNULL)
        else:
            result = self.in_process(args, dict(self.env, **env))
        self.records = [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []
        self.calls = [record["args"] for record in self.records]
        self.output = result.stdout + result.stderr
        for secret in (t.PASSWORD, t.SECRET, "synthetickey"):  # Nothing ever prints or passes a credential.
            self.assertNotIn(secret, self.output)
            self.assertNotIn(secret, json.dumps(self.calls))
        for record in self.records[2:]:  # After the endpoint check every call is pinned to it.
            self.assertEqual(record["docker_host"], self.state["endpoint"])
        for record in self.records:  # Docker never sees the caller's environment.
            passed = {key for key in record["environment"]
                      if key.startswith(("KOMIKA_", "COMPOSE_", "DOCKER_", "PGBACKREST_", "CALLER_"))}
            self.assertLessEqual(passed, {"DOCKER_HOST"})
        return result

    def mutating(self):
        return [call for call in self.calls if not any(call[:len(prefix)] == prefix for prefix in READ_ONLY)
                and not (call[0] == "compose" and ("config" in call or "version" in call))
                and not identity_call(call)]

    def refused(self, *args, status=1, shell=False, **env):
        result = self.run_rehearsal(*args, shell=shell, **env)
        self.assertEqual(result.returncode, status, self.output)
        self.assertEqual(self.mutating(), [], "a refused command reached Docker")
        return result

    def ok(self, *args, shell=False, **env):
        result = self.run_rehearsal(*args, shell=shell, **env)
        self.assertEqual(result.returncode, 0, self.output)
        return result


class UsageTests(Harness):
    def test_third_check_r1_commands_are_not_expressible(self):
        commands = [
            # The checker's exact inputs.
            ["run", "--rm", "-v", PRODUCTION_VOLUME + ":/x", "--user", "0", "--entrypoint", "/bin/sh", "postgres",
             "-c", "rm -f /x/komika.sqlite3"],
            ["exec", "-T", "--user", "postgres", "postgres", "pgbackrest", "--stanza=komika", "--repo1-path=/komika",
             "--recurse", "repo-rm", "/"],
            ["run", "--rm", "-v", "/var/run/docker.sock:/var/run/docker.sock", "postgres"],
            ["run", "--publish", "5432:5432", "postgres"],
            ["exec", "--privileged", "postgres", "sh"],
            ["run", "--privileged", "postgres"],
            # Every other former pass-through form.
            ["up", "-d", "postgres"], ["up", "--build"], ["up", "postgres"], ["up", "--with-scheduler=true"],
            ["down", "-v"], ["down", "--remove-orphans"], ["down", "--rmi", "all"], ["rm", "-fsv"],
            ["config", "--quiet"], ["-p", "komika", "up"], ["--project-name=komika", "down"], ["-f", "other.yml", "up"],
            ["--env-file", "/etc/passwd", "up"], ["--project-directory", "/", "up"], ["compose", "up"], ["ps"], ["cp"],
            ["exec", "postgres", "psql"], ["psql-check", "-c", "DROP TABLE users"], ["status", "extra"],
            ["restore", "/abs/target"], ["cleanup-restore", "komika-server-1", "komika_komika"], ["pitr-drill", "/x"],
            ["restore-cleanup", "komika-server-1"], ["restore-cleanup", "--target"],
            ["backup"], ["backup", "expire"], ["backup", "full", "--repo1-path=/komika"], ["backup", "full", "incr"],
            ["backup", "incr", "--stanza=komika"], ["info", "--repo=2"], ["info", "--output=json"],
            ["stanza-create", "--stanza=komika"], ["stanza-delete"], ["repo-rm", "/"], ["check", "--pg1-path=/x"],
            ["repo-destroy"], ["repo-destroy", "--yes-delete-rehearsal-repo", "/komika"],
            ["repo-destroy", "--yes-delete-rehearsal-repo", "--repo1-path=/komika"],
            ["destroy-source-volumes"], ["destroy-source-volumes", "--yes-delete-rehearsal-volumes", "komika_server-data"],
            ["logs", "komika-server-1"], ["logs", "postgres", "--follow"], ["logs", "postgres", "postgres-backup"],
            ["import", "--sqlite", "/a", "--sqlite", "/b", "--server-bin", "/c"], ["import", "--sqlite", "/a"],
            ["import", "--sqlite=/a", "--server-bin=/c"], ["verify", "--sqlite", "/a", "--server-bin", "/c", "--postgres", "postgres://x"],
            ["restore", "--tar", "/x", "--json-out", "/y"], ["restore", "--target", "/x"],
            ["restore", "--target", "/x", "--json-out", "/y", "--hosted-recovery"],
            ["drill", "--target", "/x", "--json-out", "/y", "--keep"], ["sql-marker"], ["sql-marker", "--label"],
            ["record-settings"], ["preflight", "--yes"], ["PREFLIGHT"], [""], [],
            # Fix round 4: the new flags take exactly one value each and exist for `restore` only.
            ["restore", "--target", "/x", "--json-out", "/y", "--expect-marker"],
            ["restore", "--target", "/x", "--json-out", "/y", "--expect-no-marker"],
            ["restore", "--target", "/x", "--target", "/z", "--json-out", "/y"],
            ["drill", "--target", "/x", "--json-out", "/y", "--expect-marker", "a"],
            ["verify-restored", "--target", "/x", "--sqlite", "/a", "--server-bin", "/b", "--expect-marker", "a"],
            ["up", "--network", "bridge"], ["up", "--local-s3"], ["status", "--watch-free"], ["down", "--force"],
            ["stop-source", "--jobs"], ["repo-destroy", "--yes-delete-rehearsal-repo", "--network=bridge"],
        ]
        for index, command in enumerate(commands):
            with self.subTest(command=" ".join(command)[:60]):
                # The checker's six inputs go through the real shell entry point.
                result = self.run_rehearsal(*command, shell=index < 6)
                self.assertEqual(result.returncode, 2, self.output)
                self.assertIn("rehearsal usage error", result.stderr)
                self.assertEqual(self.calls, [], "a usage error must not call Docker at all")

    def test_old_entry_points_never_run_a_rehearsal(self):
        legacy = dict(KOMIKA_COMPOSE_FILE=str(self.kit / "rehearsal-compose.yml"), KOMIKA_COMPOSE_PROJECT=t.PROJECT)
        for script, args in (("restore.sh", [str(self.root / "new"), "--keep"]),
                             ("restore.sh", [str(self.root / "new"), "--hosted-recovery"]),
                             ("pitr-drill.sh", [str(self.root / "new")]),
                             ("pitr-drill.sh", ["record-settings", "--out", str(self.root / "s.json"), "--hosted-recovery"])):
            for env in (legacy, {}, {"KOMIKA_COMPOSE_FILE": legacy["KOMIKA_COMPOSE_FILE"], "KOMIKA_REHEARSAL_PROJECT": ""}):
                with self.subTest(script=script, args=args[-1], env=sorted(env)):
                    result = self.run_rehearsal(*args, script=script, **env)
                    self.assertEqual(result.returncode, 1)
                    self.assertIn("only through rehearsal.sh", result.stderr)
                    self.assertEqual(self.calls, [])
                    self.assertFalse((self.root / "new").exists())
        result = subprocess.run([sys.executable, "-B", str(self.kit / "safety.py"), "up", "-d"], env=self.env,
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 1)
        self.assertIn("no host command", result.stderr)

    def test_deploy_refuses_rehearsal_before_any_docker_call(self):
        alias = self.base / "alias.yml"
        alias.symlink_to(self.kit / "rehearsal-compose.yml")
        for env in ({"KOMIKA_REHEARSAL_PROJECT": ""}, {"KOMIKA_REHEARSAL_ROOT": "/tmp/test"},
                    {"COMPOSE_FILE": str(self.kit / "rehearsal-compose.yml")},
                    {"COMPOSE_FILE": str(alias)}, {"KOMIKA_COMPOSE_FILE": str(alias)}):
            environment = {k: v for k, v in self.env.items() if not k.startswith("KOMIKA_REHEARSAL_")}
            self.state_file.write_text(json.dumps(self.state))
            result = subprocess.run(["bash", str(self.kit.parent / "deploy.sh"), "destroy"],
                                    env=dict(environment, **env), capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(self.log.exists())


class SelectionTests(Harness):
    def test_docker_and_compose_variables_and_stray_settings_are_refused(self):
        for key, value in (("DOCKER_HOST", "unix:///var/run/docker.sock"), ("DOCKER_HOST", "tcp://remote:2375"),
                           ("DOCKER_CONTEXT", "default"), ("DOCKER_CONFIG", "/x"), ("COMPOSE_FILE", "/x.yml"),
                           ("COMPOSE_PROJECT_NAME", "komika"), ("COMPOSE_PROFILES", "all"), ("COMPOSE_REMOVE_ORPHANS", "1"),
                           ("KOMIKA_COMPOSE_FILE", str(self.kit.parent / "docker-compose.yml")),
                           ("KOMIKA_COMPOSE_PROJECT", "komika"), ("KOMIKA_R2_PREFIX", "komika"),
                           ("KOMIKA_PRODUCTION_R2_PREFIX", "other"), ("KOMIKA_REHEARSAL_NO_REPO", "1"),
                           ("KOMIKA_REHEARSAL_PROJECT", "komika"), ("KOMIKA_REHEARSAL_PROJECT", "komika-pg-rehearsal-other"),
                           ("KOMIKA_COMPOSE_ENV_FILE", "relative.env"), ("KOMIKA_COMPOSE_ENV_FILE", "/nonexistent/x.env")):
            for command in (["up"], ["down"], ["backup", "incr"], ["repo-destroy", "--yes-delete-rehearsal-repo"])[::2 if key.startswith("K") else 1]:
                with self.subTest(key=key, value=value, command=command[0]):
                    self.refused(*command, **{key: value})
                    self.assertFalse(any(call[0] == "compose" for call in self.calls))

    def test_only_a_local_socket_of_the_default_context_is_used(self):
        self.state["endpoint"] = "tcp://remote:2375"
        self.refused("up")
        self.assertEqual([call[:2] for call in self.calls], [["context", "show"], ["context", "inspect"]])
        self.state["endpoint"] = "ssh://ubuntu@production"
        self.refused("stop-source")
        self.state["endpoint"] = "unix://" + str(self.env_file)  # Exists, but is no socket.
        self.refused("up")
        self.state["endpoint"] = "unix://" + str(self.base / "missing.sock")
        self.refused("up")
        self.state["endpoint"] = "unix://" + str(self.socket_path)
        self.state["context"] = "remote; rm -rf /"
        self.refused("up")
        self.state["context"] = "production"
        # A developer machine whose daemon lives in a VM may use its own context.
        with patch.object(safety, "PLATFORM", "darwin"):
            self.ok("up")
        # On the Linux target only `default` is accepted (injected, so this runs everywhere).
        self.linux()
        result = self.refused("up")
        self.assertIn("only the default local Docker context", result.stderr)
        self.assertEqual([call[:2] for call in self.calls], [["context", "show"], ["context", "inspect"]])
        self.state["context"] = "default"
        self.ok("up")
        if sys.platform.startswith("linux"):  # And through the real shell entry point on the target.
            self.state["context"] = "production"
            self.refused("up", shell=True)

    def test_env_file_must_be_private_strict_and_name_the_same_project(self):
        self.write_env(t.VALUES, mode=0o644)
        self.refused("up")
        self.write_env(dict(t.VALUES, COMPOSE_PROJECT_NAME="komika"))
        self.refused("up")
        self.write_env(dict(t.VALUES, KOMIKA_REHEARSAL_PROJECT="komika-pg-rehearsal-other"))
        self.refused("up")
        self.write_env({k: v for k, v in t.VALUES.items() if k != "KOMIKA_PRODUCTION_R2_PREFIX"})
        self.refused("up")
        for rehearsal, production in (("rehearsal/test", "rehearsal/test"), ("rehearsal/test", "rehearsal"),
                                      ("rehearsal/test/child", "rehearsal/test"), ("komika", "production")):
            self.write_env(dict(t.VALUES, KOMIKA_R2_PREFIX=rehearsal, KOMIKA_PRODUCTION_R2_PREFIX=production))
            self.refused("backup", "incr")
            self.assertFalse(any(call[0] == "compose" for call in self.calls))


class TopologyTests(Harness):
    def test_effective_configuration_is_compared_before_every_subcommand(self):
        from test_safety import UNSAFE
        subcommands = (["up"], ["status"], ["psql-check"], ["backup", "incr"], ["stop-source"], ["down"],
                       ["logs"], ["info"], ["sql-marker", "--label", "a"], ["repo-destroy", "--yes-delete-rehearsal-repo"])
        for index, (name, mutate) in enumerate(UNSAFE.items()):
            command = subcommands[index % len(subcommands)]
            with self.subTest(change=name, command=command[0]):
                self.state["config"] = t.rendered()
                mutate(self.state["config"])
                result = self.refused(*command, shell=index < 3)
                self.assertIn("rehearsal topology", result.stderr)
                self.assertEqual([call[0] for call in self.calls], ["context", "context", "compose"])

    def copied_kit(self, edit=None, repin=False):
        copy = self.base / "kit"
        if copy.exists():
            shutil.rmtree(copy)
        copy.mkdir()
        for name in ("rehearsal.sh", "rehearsal.py", "drill.py", "rehearsal-compose.yml") + safety.IMAGE_FILES:
            shutil.copy(safety.KIT / name, copy / name)
        if edit:
            compose = copy / "rehearsal-compose.yml"
            text = compose.read_text()
            self.assertEqual(text.count(edit[0]), 1)
            compose.write_text(text.replace(*edit))
            if repin:
                import hashlib
                source = (copy / "safety.py").read_text()
                digest = hashlib.sha256(compose.read_bytes()).hexdigest()
                (copy / "safety.py").write_text(source.replace(safety.COMPOSE_SHA256, digest))
        return copy

    def test_modified_compose_file_is_refused_by_the_pin_and_then_by_the_topology(self):
        from test_safety import UNSAFE
        volumes = "    volumes: &volumes\n"
        edits = {
            "extra bind mount of a production volume": (volumes, volumes + f"      - {PRODUCTION_VOLUME}:/x\n"),
            "privileged": ("    init: true\n    restart: \"no\"\n    stop_grace_period: 240s\n    mem_limit: ${KOMIKA_REHEARSAL_PG_MEMORY:-3g}\n",
                           "    init: true\n    privileged: true\n    restart: \"no\"\n    stop_grace_period: 240s\n    mem_limit: ${KOMIKA_REHEARSAL_PG_MEMORY:-3g}\n"),
            "host network mode": ("    tmpfs: &tmpfs\n", "    network_mode: host\n    tmpfs: &tmpfs\n"),
            "published port": ("    tmpfs: &tmpfs\n", "    ports: ['5432:5432']\n    tmpfs: &tmpfs\n"),
            "volumes_from": ("    command: [scheduler]\n", "    command: [scheduler]\n    volumes_from: [postgres]\n"),
            "volume driver_opts": ("    name: ${KOMIKA_REHEARSAL_PROJECT:?set rehearsal project}-postgres-data\n    driver: local\n",
                                   "    name: ${KOMIKA_REHEARSAL_PROJECT:?set rehearsal project}-postgres-data\n    driver: local\n"
                                   f"    driver_opts: {{type: none, o: bind, device: {PRODUCTION_VOLUME}}}\n"),
            "extra network on a service": ("    networks: [rehearsal]\n    depends_on:\n", "    networks: [rehearsal, komika]\n    depends_on:\n"),
            "changed image": ("    image: komika-postgres:18.6-pgbackrest-2.59\n    command: [scheduler]\n",
                              "    image: postgres:18\n    command: [scheduler]\n"),
            "prefix variable missing in postgres-backup": ("    environment: *environment\n",
                                                           "    environment: {PGDATA: /var/lib/postgresql/18/docker}\n"),
        }
        self.kit = self.copied_kit()
        self.ok("status")  # Control: an unmodified copy of the kit works.
        for name, edit in edits.items():
            with self.subTest(change=name, stage="pin"):
                self.kit = self.copied_kit(edit)
                self.state["config"] = t.rendered()  # Even a rendering that still looks perfect.
                result = self.refused("up")
                self.assertIn("pinned", result.stderr)
                self.assertFalse(any(call[0] == "compose" for call in self.calls))
            with self.subTest(change=name, stage="topology"):
                self.kit = self.copied_kit(edit, repin=True)
                self.state["config"] = t.rendered()
                UNSAFE[name](self.state["config"])  # What Compose renders for that edit.
                result = self.refused("up")
                self.assertIn("rehearsal topology", result.stderr)
        self.kit = safety.KIT

    def test_existing_resources_without_our_label_are_never_touched(self):
        unlabelled = t.container()
        del unlabelled["Config"]["Labels"][t.LABEL]
        foreign = dict(t.container(), Name="/unexpected", Id="e" * 64)
        foreign["Config"] = dict(foreign["Config"], Labels={"com.docker.compose.project": t.PROJECT})
        cases = {
            "container": {"containers": [unlabelled]},
            "volume": {"volumes": [volume(t.PROJECT + "-postgres-data", labelled=False)]},
            "network": {"networks": [network(t.PROJECT + "-network", "172.30.21.0/24", labelled=False)]},
            "project-labelled stranger": {"containers": [foreign]},
        }
        for name, change in cases.items():
            for command in (["up"], ["destroy-source-volumes", "--yes-delete-rehearsal-volumes"], ["status"],
                            ["backup", "incr"], ["repo-destroy", "--yes-delete-rehearsal-repo"]):
                with self.subTest(resource=name, command=command[0]):
                    saved = dict(self.state)
                    self.state.update(change)
                    result = self.refused(*command)
                    self.assertIn("lacks the selected rehearsal label", result.stderr)
                    self.state = saved
            # stop-source and down leave exactly that object alone, say so, and still
            # remove what is the rehearsal's own (fix round 4, F2).
            for command in ("stop-source", "down"):
                with self.subTest(resource=name, command=command):
                    saved = dict(self.state)
                    self.state.update(change)
                    stranger = [e["Id"] for e in self.state["containers"]
                                if e["Config"]["Labels"].get(t.LABEL) != t.PROJECT]
                    result = self.run_rehearsal(command)
                    self.assertEqual(result.returncode, 1, self.output)
                    self.assertIn("REFUSED: ", result.stdout)
                    self.assertIn("lacks the selected rehearsal label", result.stdout)
                    touched = json.dumps(self.mutating())
                    for identity in stranger:
                        self.assertNotIn(identity, touched)
                    self.assertNotIn("volume", touched)
                    removed_network = any(call[:2] == ["network", "rm"] for call in self.mutating())
                    self.assertEqual(removed_network, command == "down" and name != "network")
                    self.state = saved

    def test_live_container_must_equal_the_contract_before_any_exec(self):
        deviations = {
            "privileged": lambda e: e["HostConfig"].update(Privileged=True),
            "production bind": lambda e: e["Mounts"].append({"Type": "bind", "Source": PRODUCTION_VOLUME, "Destination": "/x"}),
            "host network": lambda e: e["HostConfig"].update(NetworkMode="host"),
            "published port": lambda e: e["HostConfig"].update(PortBindings={"5432/tcp": [{"HostPort": "5432"}]}),
            "production prefix": lambda e: e["Config"].update(
                Env=[i if not i.startswith("KOMIKA_R2_PREFIX=") else "KOMIKA_R2_PREFIX=komika" for i in e["Config"]["Env"]]),
            # C2: the states the check accepted before fix round 5.
            "memory limit removed": lambda e: e["HostConfig"].update(Memory=0, MemorySwap=0),
            "swap unlimited": lambda e: e["HostConfig"].update(MemorySwap=-1),
            "CPU limit removed": lambda e: e["HostConfig"].update(NanoCpus=0),
            "PID limit removed": lambda e: e["HostConfig"].update(PidsLimit=0),
            "shm raised": lambda e: e["HostConfig"].update(ShmSize=8 << 30),
            "unbounded logging": lambda e: e["HostConfig"].update(LogConfig={"Type": "json-file", "Config": {}}),
            "restart always": lambda e: e["HostConfig"].update(RestartPolicy={"Name": "always"}),
            "libpq variables": lambda e: e["Config"]["Env"].extend(["PGHOST=db.production.invalid", "PGPORT=5432",
                                                                     "PGPASSWORD=notarealpassword"]),
            "pgBackRest variable": lambda e: e["Config"]["Env"].append("PGBACKREST_REPO1_PATH=/komika"),
            "kit variable": lambda e: e["Config"]["Env"].append("KOMIKA_UNKNOWN=1"),
            # C1: the volume name and the destination are right, the bytes are production's.
            "mount source changed": lambda e: e["Mounts"][0].update(Source=PRODUCTION_VOLUME),
        }
        commands = (["psql-check"], ["stanza-create"], ["backup", "full", "--first"], ["check"], ["info"],
                    ["archive-status"], ["sql-marker", "--label", "a"], ["status"],
                    ["record-settings", "--out", str(self.root / "s.json")],
                    ["import", "--sqlite", str(self.env_file), "--server-bin", str(self.env_file)])
        for index, (name, deviate) in enumerate(deviations.items()):
            # Every command meets a deviating container; every deviation meets four commands.
            for command in (commands if index == 0 else commands[index % 3::3]):
                with self.subTest(deviation=name, command=command[0]):
                    self.state["containers"] = [t.container()]
                    deviate(self.state["containers"][0])
                    result = self.refused(*command)
                    self.assertRegex(result.stderr, "does not match the rehearsal contract|is not rehearsal storage")
                    self.assertNotIn("notarealpassword", self.output)
                    self.assertNotIn("db.production.invalid", self.output)
        # `up` meets the same container after Compose: a container that Compose left as it was is refused too.
        for name in ("memory limit removed", "CPU limit removed", "libpq variables"):
            with self.subTest(deviation=name, command="up"):
                self.state["containers"] = [t.container()]
                deviations[name](self.state["containers"][0])
                result = self.run_rehearsal("up")
                self.assertEqual(result.returncode, 1, self.output)
                self.assertIn("does not match the rehearsal contract", result.stderr)
                self.assertFalse(any(call[0] == "exec" for call in self.calls))


class CommandTests(Harness):
    COMPOSE = None

    def compose(self):
        return ["compose", "--env-file", str(self.env_file), "-f", str(self.ran / "rehearsal-compose.yml"), "-p", t.PROJECT]

    def test_up_renders_first_and_builds_its_own_exact_command(self):
        self.ok("up", shell=True)
        self.assertEqual([call[:2] for call in self.calls[:2]], [["context", "show"], ["context", "inspect"]])
        self.assertEqual(self.calls[2], self.compose() + ["config", "--format", "json"])
        self.assertEqual(self.mutating(), [self.compose() + ["up", "-d", "--no-build", "--pull", "never", "postgres"]])
        self.assertLess(self.calls.index(self.compose() + ["config", "--format", "json"]), self.calls.index(self.mutating()[0]))
        self.state["containers"].append(t.container("postgres-backup"))
        self.ok("up", "--with-scheduler")
        self.assertEqual(self.mutating(), [self.compose() + ["up", "-d", "--no-build", "--pull", "never", "postgres", "postgres-backup"]])
        self.state["image"] = False
        result = self.refused("up")  # Never builds, never pulls.
        self.assertIn("not present locally", result.stderr)

    def test_repository_work_goes_through_the_checked_wrappers_only(self):
        identity = "a" * 64
        expected = {
            ("stanza-create",): ["/opt/komika-postgres/pgbackrest.sh", "stanza-create"],
            ("backup", "full", "--first"): ["/opt/komika-postgres/backup.sh", "bootstrap-full"],
            ("backup", "full"): ["/opt/komika-postgres/backup.sh", "full"],
            ("backup", "incr"): ["/opt/komika-postgres/backup.sh", "incr"],
            ("check",): ["/opt/komika-postgres/backup.sh", "check"],
            ("info",): ["/opt/komika-postgres/pgbackrest.sh", "--output=text", "info"],
            ("info", "--json"): ["/opt/komika-postgres/pgbackrest.sh", "--output=json", "info"],
        }
        for command, recipe in expected.items():
            with self.subTest(command=command):
                self.ok(*command)
                self.assertEqual(self.mutating(), [["exec", "--user", "postgres", identity] + recipe])
        self.refused("backup", "incr", "--first", status=2)
        # No subcommand ever runs the raw binary or a caller-chosen executable.
        allowed = {"/opt/komika-postgres/pgbackrest.sh", "/opt/komika-postgres/backup.sh", "/opt/komika-postgres/health.sh",
                   "psql", "sh", "pg_ctl"}
        for command in (["status"], ["psql-check"], ["archive-status"], ["sql-marker", "--label", "before-T"]):
            self.state["exec"] = {"pg_database": "1", "ON CONFLICT": "before-T"}
            self.ok(*command)
            for call in self.mutating():
                self.assertEqual(call[:4], ["exec", "--user", "postgres", identity])
                self.assertIn(call[4], allowed)

    def test_sql_marker_uses_its_own_database_and_a_strict_label(self):
        self.state["exec"] = {"pg_database": "1", "ON CONFLICT": "before-T", "database_time": "database_time=2026-10-11T00:00:00.000000Z"}
        result = self.ok("sql-marker", "--label", "before-T")
        self.assertIn("marker=before-T", result.stdout)
        statements = [(call[call.index("-d") + 1], call[-1]) for call in self.mutating()]
        self.assertTrue(all(database == "komika_pitr_drill" or statement.startswith("SELECT") for database, statement in statements))
        self.assertTrue(any("INSERT INTO operator_markers(label) VALUES ('before-T')" in statement for _, statement in statements))
        for label in ("x'; DROP TABLE users; --", "a b", "", "x" * 41, "a\nb", "$(id)"):
            with self.subTest(label=label):
                self.refused("sql-marker", "--label", label)

    def test_logs_take_a_fixed_service_and_a_number(self):
        self.ok("logs")
        self.assertEqual(self.mutating(), [["logs", "--tail", "200", "a" * 64]])
        self.state["containers"].append(t.container("postgres-backup"))
        self.ok("logs", "postgres-backup", "--tail", "50")
        self.assertEqual(self.mutating(), [["logs", "--tail", "50", "b" * 64]])
        for tail in ("0", "all", "5 --follow", "-1", "100000"):
            self.refused("logs", "--tail", tail)

    def test_stop_destroy_and_down_touch_only_labelled_fixed_names(self):
        self.state["containers"].append(t.container("postgres-backup"))
        self.ok("stop-source")
        self.assertEqual(self.mutating(), [["stop", "-t", "240", "b" * 64], ["rm", "b" * 64],
                                           ["stop", "-t", "240", "a" * 64], ["rm", "a" * 64]])
        self.ok("down")
        self.assertEqual(self.mutating()[-1], ["network", "rm", self.state["networks"][0]["Id"]])
        self.assertFalse(any(call[:2] == ["volume", "rm"] or "-v" in call for call in self.mutating()))
        result = self.refused("destroy-source-volumes", "--yes-delete-rehearsal-volumes")
        self.assertIn("stop-source", result.stderr)
        self.state["containers"] = []
        self.ok("destroy-source-volumes", "--yes-delete-rehearsal-volumes")
        self.assertEqual(self.mutating(), [["volume", "rm", t.PROJECT + "-postgres-data"],
                                           ["volume", "rm", t.PROJECT + "-postgres-socket"]])
        self.assertNotIn("komika_server-data", json.dumps(self.mutating()))

    def test_repo_destroy_runs_one_fixed_recipe_without_mounts_or_paths(self):
        result = self.refused("repo-destroy", "--yes-delete-rehearsal-repo")
        self.assertIn("stop-source", result.stderr)  # Source still running.
        self.state["containers"] = []
        result = self.ok("repo-destroy", "--yes-delete-rehearsal-repo")
        self.assertIn("rehearsal prefix rehearsal/test", result.stdout)
        runs = self.mutating()
        self.assertEqual(len(runs), 2)
        destroy = runs[0]
        self.assertEqual(destroy[destroy.index(safety.IMAGE):],
                         [safety.IMAGE, "exec", "sh", "-c", "/opt/komika-postgres/pgbackrest.sh stop && "
                          "/opt/komika-postgres/pgbackrest.sh --force stanza-delete"])
        self.assertEqual(runs[1][-5:], ["exec", "/opt/komika-postgres/pgbackrest.sh", "--recurse", "--output=json", "repo-ls"])
        for run, record in zip(runs, [r for r in self.records if r["args"][0] == "run" and not identity_call(r["args"])]):
            # Only the rehearsal's own bridge, never Docker's default bridge (F8).
            self.assertIn(f"--network={t.PROJECT}-network", run)
            self.assertFalse(any(arg in ("bridge", "host") or arg.endswith(("=bridge", "=host")) for arg in run))
            self.assertIn(f"{t.LABEL}={t.PROJECT}", run)
            for option in ("--log-driver=json-file", "--log-opt=max-size=10m", "--log-opt=max-file=3",
                           "--memory=512m", "--memory-swap=512m", "--cpus=1", "--pids-limit=128"):
                self.assertEqual(run[:run.index(safety.IMAGE)].count(option), 1, option)  # C8.
            self.assertTrue(re.fullmatch(re.escape(t.PROJECT) + "-meta-[0-9a-f]{8}", run[run.index("--name") + 1]))
            self.assertFalse(any(arg in ("-v", "--volume", "--mount", "--volumes-from", "--privileged", "-p", "--publish")
                                 or arg.startswith(("--repo", "--mount", "--volume")) for arg in run))
            # The image receives the comparator, so its own fence runs again: the VALUES
            # that reach the container are the env file's rehearsal prefix and project.
            self.assertEqual(record["stdin_values"], {
                "KOMIKA_R2_PREFIX": "rehearsal/test", "KOMIKA_PRODUCTION_R2_PREFIX": "komika",
                "KOMIKA_REHEARSAL_PROJECT": t.PROJECT, "KOMIKA_R2_VERIFY_TLS": "1",
                "KOMIKA_R2_BUCKET": "synthetic-bucket", "KOMIKA_R2_ENDPOINT": "example.r2.invalid"})
        # Another env file: that file's prefix arrives, nothing remembered or defaulted.
        self.write_env(dict(t.VALUES, KOMIKA_R2_PREFIX="rehearsal/other-run", KOMIKA_PRODUCTION_R2_PREFIX="live/komika"))
        for service in self.state["config"]["services"].values():
            service["environment"].update(KOMIKA_R2_PREFIX="rehearsal/other-run", KOMIKA_PRODUCTION_R2_PREFIX="live/komika")
        result = self.ok("repo-destroy", "--yes-delete-rehearsal-repo")
        self.assertIn("rehearsal prefix rehearsal/other-run", result.stdout)
        for record in self.records:
            if record["args"][0] == "run" and not identity_call(record["args"]):
                self.assertEqual(record["stdin_values"]["KOMIKA_R2_PREFIX"], "rehearsal/other-run")
                self.assertEqual(record["stdin_values"]["KOMIKA_PRODUCTION_R2_PREFIX"], "live/komika")
        # Without the rehearsal network there is no reader at all (it never falls back to the default bridge).
        self.state["networks"] = [network("komika_komika", "172.21.0.0/16", labelled=False)]
        result = self.refused("repo-destroy", "--yes-delete-rehearsal-repo")
        self.assertIn("rehearsal network", result.stderr)
        self.refused("info")

    def test_no_repository_mode_cannot_reach_any_repository_command(self):
        self.no_repository()
        self.ok("status")
        self.ok("psql-check")
        self.ok("up")
        for command in (["stanza-create"], ["backup", "full", "--first"], ["backup", "incr"], ["check"], ["info"],
                        ["up", "--with-scheduler"], ["repo-destroy", "--yes-delete-rehearsal-repo"],
                        ["drill", "--target", str(self.root / "d"), "--json-out", str(self.root / "d.json")],
                        ["restore", "--target", str(self.root / "r"), "--json-out", str(self.root / "r.json")]):
            with self.subTest(command=command):
                result = self.refused(*command)
                self.assertIn("NO_REPO", result.stderr)
        # Any repository value beside NO_REPO=1 is refused outright.
        self.write_env(dict(t.NO_REPO_VALUES, KOMIKA_R2_PREFIX="rehearsal/test"))
        self.refused("up")
        self.write_env(dict(t.NO_REPO_VALUES, KOMIKA_PRODUCTION_R2_PREFIX="komika"))
        self.refused("status")


class JobTests(Harness):
    def setUp(self):
        super().setUp()
        self.sqlite = self.base / "copy.sqlite3"
        self.sqlite.write_bytes(b"SQLite format 3\0")
        self.binary = self.base / "komika-server"
        self.binary.write_bytes(b"\x7fELF")
        self.binary.chmod(0o755)

    def job(self, *extra):
        return ["--sqlite", str(self.sqlite), "--server-bin", str(self.binary), *extra]

    def check_run(self, run, record, command, network_name, host):
        work = [arg for arg in run if arg.startswith("type=bind,src=") and arg.endswith(",dst=/work")]
        self.assertEqual(len(work), 1)
        directory = Path(work[0][len("type=bind,src="):-len(",dst=/work")])
        self.assertEqual(directory.parent, self.root)
        # A root caller still runs the job as nobody, in a work directory given to it.
        uid, gid = (os.getuid(), os.getgid()) if os.getuid() else (65534, 65534)
        self.assertEqual((directory.stat().st_uid, directory.stat().st_gid), (uid, gid))
        self.assertEqual(run, [
            "run", "--rm", "--init", "--pull=never", "--name", run[5], "--label", f"{t.LABEL}={t.PROJECT}",
            "--network", network_name, "--user", f"{uid}:{gid}", "--read-only", "--cap-drop=ALL",
            "--security-opt=no-new-privileges", "--memory=2g", "--memory-swap=2g", "--cpus=1", "--pids-limit=256",
            "--log-driver=json-file", "--log-opt=max-size=10m", "--log-opt=max-file=3",  # C8: bounded, explicit.
            "--env-file", "/dev/stdin", "--workdir", "/work",
            "--mount", f"type=bind,src={self.binary},dst=/opt/komika-server/komika-server,readonly",
            "--mount", f"type=bind,src={self.sqlite},dst=/source/komika.sqlite3,readonly",
            "--mount", work[0],
            "--tmpfs", f"/source:rw,noexec,nosuid,nodev,size=64m,mode=0700,uid={uid},gid={gid}",
            "--tmpfs", "/var/lib/postgresql:mode=0700",
            "--entrypoint", "/opt/komika-server/komika-server",
            safety.IMAGE, command, "--sqlite", "/source/komika.sqlite3", "--json-out", "/work/report.json"])
        self.assertTrue(re.fullmatch(re.escape(t.PROJECT) + "-job-[0-9a-f]{8}", run[5]))
        self.assertNotIn("0:0", run)
        # The database URL (with the password) travels on stdin only.
        self.assertEqual(record["stdin_keys"], ["HOME", "KOMIKA_IMPORT_POSTGRES_URL", "SQLITE_TMPDIR", "TMPDIR"])
        self.assertTrue(record["stdin_has_password"])
        self.assertNotIn(host + ":5432", json.dumps(run))
        return directory

    def test_import_and_verify_run_read_only_non_root_on_the_rehearsal_network(self):
        for subcommand, command in (("import", "import-sqlite-to-postgres"), ("verify", "verify-sqlite-postgres")):
            with self.subTest(subcommand=subcommand):
                report = self.root / f"{subcommand}-report.json"
                result = self.ok(subcommand, *self.job("--report", str(report)), shell=subcommand == "import")
                runs = [record for record in self.records if record["args"][0] == "run" and not identity_call(record["args"])]
                self.assertEqual(len(runs), 1)
                self.assertEqual(self.mutating(), [runs[0]["args"]])
                directory = self.check_run(runs[0]["args"], runs[0], command, t.PROJECT + "-network", t.PROJECT + "-postgres")
                self.assertEqual(report.read_text(), '{"complete": true}')
                self.assertEqual(stat_mode(report), 0o600)
                self.assertEqual(stat_mode(directory), 0o700)
                self.assertIn(f"work_directory={directory}", result.stdout)
        self.state["run_status"] = 3
        self.assertEqual(self.run_rehearsal("import", *self.job()).returncode, 3)  # The job's status is the result.

    def test_job_inputs_and_outputs_are_fenced(self):
        elsewhere = self.base / "elsewhere.json"
        (self.root / "taken.json").write_text("{}")
        link = self.base / "link.sqlite3"
        link.symlink_to(self.sqlite)
        cases = {
            "report outside the root": self.job("--report", str(elsewhere)),
            "report already exists": self.job("--report", str(self.root / "taken.json")),
            "report below a subdirectory": self.job("--report", str(self.root / "sub" / "r.json")),
            "report with mount syntax": self.job("--report", str(self.root / "r,x")),
            "relative sqlite": ["--sqlite", "copy.sqlite3", "--server-bin", str(self.binary)],
            "symlinked sqlite": ["--sqlite", str(link), "--server-bin", str(self.binary)],
            "missing sqlite": ["--sqlite", str(self.base / "none.sqlite3"), "--server-bin", str(self.binary)],
            "sqlite with mount option": ["--sqlite", str(self.sqlite) + ",readonly=false", "--server-bin", str(self.binary)],
            "directory as binary": ["--sqlite", str(self.sqlite), "--server-bin", str(self.base)],
            "dotted path": ["--sqlite", str(self.base / "bin" / ".." / "copy.sqlite3"), "--server-bin", str(self.binary)],
        }
        for name, arguments in cases.items():
            with self.subTest(case=name):
                self.refused("import", *arguments)
                self.assertFalse(elsewhere.exists())
        for path, mode, restore in ((self.sqlite, 0o200, 0o644), (self.binary, 0o644, 0o755), (self.binary, 0o311, 0o755)):
            path.chmod(mode)
            self.assertIn("by the job user", self.refused("import", *self.job()).stderr)
            path.chmod(restore)
        self.assertEqual(sorted(p.name for p in self.root.iterdir()), [safety.ROOT_MARKER, "taken.json"])
        (self.base / "copy.sqlite3-wal").write_bytes(b"pending pages")
        result = self.refused("import", *self.job())
        self.assertIn("-wal", result.stderr)
        (self.base / "copy.sqlite3-wal").write_bytes(b"")  # An empty log holds nothing.
        self.ok("verify", *self.job())
        (self.base / "copy.sqlite3-journal").write_bytes(b"hot journal")
        self.refused("verify", *self.job())
        (self.base / "copy.sqlite3-journal").unlink()
        self.state["containers"][0]["State"].update(Running=False, Status="exited")
        result = self.refused("import", *self.job())
        self.assertIn("not running", result.stderr)

    def test_rehearsal_root_is_fenced_before_any_directory_or_container(self):
        before = sorted(os.listdir(self.root))
        (self.root / safety.ROOT_MARKER).unlink()
        result = self.refused("import", *self.job())
        self.assertIn("marker", result.stderr)
        (self.root / safety.ROOT_MARKER).write_text(safety.marker_text(os.getuid()))
        self.root.chmod(0o755)
        self.refused("verify", *self.job())
        self.root.chmod(0o700)
        # Root inside Docker's data root, and around a production mount source.
        self.rehome(self.base)
        result = self.refused("import", *self.job())
        self.assertIn("Docker data root", result.stderr)
        self.rehome(self.base / "docker-data")
        production = dict(t.container(), Name="/komika-server-1", Id="d" * 64)
        production["Config"] = dict(production["Config"], Labels={"com.docker.compose.project": "komika"})
        production["Mounts"] = [{"Type": "bind", "Source": str(self.root / "data"), "Destination": "/data"}]
        self.state["containers"].append(production)
        result = self.refused("record-settings", "--out", str(self.root / "s.json"))
        self.assertIn("komika-server-1", result.stderr)
        self.assertEqual(sorted(os.listdir(self.root)), before + [])
        self.refused("import", *self.job(), KOMIKA_REHEARSAL_ROOT=str(self.base / "missing"))
        self.refused("import", *self.job(), KOMIKA_REHEARSAL_ROOT="/")
        self.refused("import", *self.job(), KOMIKA_REHEARSAL_ROOT=str(self.base))  # Exists, never initialised.


class RestoreTests(Harness):
    def kept(self, target, name="komika-pitr-0123456789ab", project=t.PROJECT):
        labels = {t.LABEL: project, safety.TARGET_LABEL: str(target)}
        entry = {"Id": "9" * 64, "Name": "/" + name, "State": {"Status": "running", "Running": True},
                 "Config": {"Labels": labels}, "Mounts": [{"Type": "bind", "Source": str(target), "Destination": "/restore"}]}
        net = {"Id": "8" * 64, "Name": name + "-network", "Labels": dict(labels), "IPAM": {"Config": [{"Subnet": "172.29.42.0/24"}]}}
        return entry, net

    def test_cleanup_and_verifier_find_a_kept_restore_by_its_directory_only(self):
        target = self.root / "pitr"
        target.mkdir()
        entry, net = self.kept(target)
        self.state["containers"].append(entry)
        self.state["networks"].append(net)
        sqlite, binary = self.base / "copy.sqlite3", self.base / "komika-server"
        sqlite.write_bytes(b"x")
        binary.write_bytes(b"x")
        binary.chmod(0o755)
        self.ok("verify-restored", "--target", str(target), "--sqlite", str(sqlite), "--server-bin", str(binary))
        run = self.mutating()[0]
        self.assertEqual(run[run.index("--network") + 1], "komika-pitr-0123456789ab-network")
        self.assertEqual(run[run.index(safety.IMAGE) + 1], "verify-sqlite-postgres")
        self.ok("restore-cleanup", "--target", str(target))
        self.assertEqual(self.mutating(), [
            ["exec", "--user", "postgres", "9" * 64, "pg_ctl", "-D", "/restore", "-w", "-m", "fast", "stop"],
            ["rm", "-f", "-v", "9" * 64], ["network", "rm", "8" * 64]])
        self.assertTrue(target.is_dir())  # The restored directory is retained.
        # Nothing with that directory label: nothing is removed, nothing else is guessed.
        self.state["containers"].remove(entry)
        self.state["networks"].remove(net)
        self.ok("restore-cleanup", "--target", str(target))
        self.assertEqual(self.mutating(), [])

    def test_cleanup_refuses_what_is_not_this_rehearsals_throwaway_restore(self):
        target = self.root / "pitr"
        target.mkdir()
        for name, change in {
            "production name": lambda e, n: e.update(Name="/komika-server-1"),
            "other mount": lambda e, n: e["Mounts"].append({"Type": "bind", "Source": PRODUCTION_VOLUME, "Destination": "/x"}),
            "production volume as restore": lambda e, n: e["Mounts"][0].update(Source=PRODUCTION_VOLUME),
            "production network name": lambda e, n: n.update(Name="komika_komika"),
        }.items():
            with self.subTest(case=name):
                entry, net = self.kept(target)
                change(entry, net)
                self.state["containers"] = [t.container(), entry]
                self.state["networks"] = [network(t.PROJECT + "-network", "172.30.21.0/24"), net]
                self.refused("restore-cleanup", "--target", str(target))
        for bad in (self.base / "elsewhere", self.root / "missing", self.root, Path("/")):
            with self.subTest(target=bad):
                self.refused("restore-cleanup", "--target", str(bad))

    def test_restore_and_drill_arguments_are_validated_before_any_repository_access(self):
        with patch.object(safety, "PLATFORM", "darwin"):
            result = self.refused("restore", "--target", str(self.root / "r"), "--json-out", str(self.root / "r.json"))
            self.assertIn("Linux Docker host", result.stderr)
            result = self.refused("drill", "--target", str(self.root / "d"), "--json-out", str(self.root / "d.json"))
            self.assertIn("Linux Docker host", result.stderr)
        self.linux()  # Everything below runs as on the target, on every platform.
        good = ["--target", str(self.root / "r"), "--json-out", str(self.root / "r.json")]
        (self.root / "settings.json").write_text("{}")
        cases = {
            "target outside the root": ["--target", str(self.base / "r"), "--json-out", str(self.root / "r.json")],
            "target is the production volume": ["--target", PRODUCTION_VOLUME + "/new", "--json-out", str(self.root / "r.json")],
            "nested target": ["--target", str(self.root / "a" / "b"), "--json-out", str(self.root / "r.json")],
            "existing target": ["--target", str(self.root), "--json-out", str(self.root / "r.json")],
            "report outside the root": ["--target", str(self.root / "r"), "--json-out", str(self.base / "r.json")],
            "local time": good + ["--target-time", "2026-10-11 12:00:00"],
            "offset time": good + ["--target-time", "2026-10-11T12:00:00+02:00"],
            "option in time": good + ["--target-time", "2026-10-11T12:00:00Z --repo1-path=/komika"],
            "impossible date": good + ["--target-time", "2026-13-41T12:00:00Z"],
            "settings outside the root": good + ["--source-settings", str(self.env_file)],
            "invalid settings record": good + ["--source-settings", str(self.root / "settings.json")],
            "marker label with SQL": good + ["--expect-marker", "x'; DROP TABLE users; --"],
            "marker label too long": good + ["--expect-no-marker", "x" * 41],
            "marker expected present and absent": good + ["--expect-marker", "a", "--expect-no-marker", "a"],
        }
        for name, arguments in cases.items():
            with self.subTest(case=name):
                self.refused("restore", *arguments)
                self.assertFalse((self.root / "r").exists())
        # A root that overlaps a retained production volume refuses before mkdir/run/create.
        self.state["volumes"].append(volume("retained", labelled=False, mountpoint=str(self.root / "old")))
        result = self.refused("restore", *good)
        self.assertIn("volume retained", result.stderr)
        self.assertFalse((self.root / "r").exists())


class PreflightTests(Harness):
    def test_preflight_reports_without_secrets_and_changes_nothing(self):
        result = self.ok("preflight", shell=True)
        self.assertEqual(result.stdout.splitlines()[-1], "PREFLIGHT OK")
        self.assertEqual(self.mutating(), [])
        for expected in (f"calling_uid={os.getuid()}", "docker_context=default", "docker_endpoint=unix://",
                         "docker_root_dir=" + self.state["docker_root"], f"rehearsal_root={self.root} owner={os.getuid()} mode=0700",
                         "rehearsal_prefix=rehearsal/test", "production_prefix=komika", f"image={safety.IMAGE} id=sha256:fake",
                         f"container {t.PROJECT}-postgres state=running compose_project={t.PROJECT} rehearsal_label={t.PROJECT}",
                         "volume komika_server-data labels={}", "network komika_komika subnets=172.21.0.0/16",
                         "subnet 172.30.21.0/24: in use by " + t.PROJECT + "-network", "subnet 172.29.42.0/24: free",
                         "sha256=" + safety.COMPOSE_SHA256, "free_bytes="):
            self.assertIn(expected, result.stdout)

    def test_preflight_lists_every_refusal(self):
        self.state["networks"].append(network("clubsite_default", "172.29.0.0/16", labelled=False))
        self.state["image"] = False
        self.state["config"]["services"]["postgres"]["privileged"] = True
        (self.root / safety.ROOT_MARKER).unlink()
        result = self.refused("preflight")
        lines = result.stdout.splitlines()
        self.assertEqual(lines[-1], "PREFLIGHT REFUSED (4)")
        refusals = "\n".join(line for line in lines if line.startswith("REFUSAL: "))
        for expected in ("effective topology", "image", "rehearsal root", "subnet 172.29.42.0/24 overlaps network clubsite_default"):
            self.assertIn(expected, refusals)
        result = self.refused("preflight", DOCKER_HOST="tcp://remote:2375")
        self.assertIn("REFUSAL: selection: DOCKER_HOST", result.stdout)

    def test_init_root_creates_only_a_new_private_marked_directory(self):
        new = self.base / "fresh-root"
        self.absent_project()  # A new root belongs to a NEW project (C1).
        result = self.ok("preflight", "--init-root", KOMIKA_REHEARSAL_ROOT=str(new))
        self.assertEqual(self.mutating(), [])
        self.assertIn("rehearsal_root_created=yes", result.stdout)
        self.assertIn(f"new_project=no container, volume or network of {t.PROJECT} exists", result.stdout)
        self.assertEqual(stat_mode(new), 0o700)
        self.assertEqual((new / safety.ROOT_MARKER).read_text(), safety.marker_text(os.getuid()))
        self.ok("preflight", KOMIKA_REHEARSAL_ROOT=str(new))
        # It never adopts an existing directory, and never creates one in a fenced place.
        existing = self.base / "existing"
        existing.mkdir(mode=0o700)
        self.refused("preflight", "--init-root", KOMIKA_REHEARSAL_ROOT=str(existing))
        self.assertEqual(os.listdir(existing), [])
        inside = self.base / "docker-data" / "rehearsal"
        self.refused("preflight", "--init-root", KOMIKA_REHEARSAL_ROOT=str(inside))
        self.assertFalse(inside.exists())
        self.refused("preflight", "--init-root", KOMIKA_REHEARSAL_ROOT=str(self.base / "a" / "b"))
        self.refused("preflight", "--init-root", KOMIKA_REHEARSAL_ROOT="relative")


GIB = 1 << 30
FLOOR = 3 * GIB


def labelled_container(name, identity, running=True, **labels):
    return {"Id": identity * 64, "Name": "/" + name, "Image": "sha256:fake",
            "State": {"Status": "running" if running else "exited", "Running": running, "StartedAt": "2026-10-11T12:00:00Z"},
            "Config": {"Labels": dict({t.LABEL: t.PROJECT}, **labels)}, "Mounts": []}


class DiskGateTests(Harness):
    """F1: no step may fill the disk of a host that also runs production."""

    def test_up_and_backup_keep_the_floor_on_each_filesystem(self):
        self.free(docker=FLOOR - 1, root=50 * GIB, same=False)
        for command in (["up"], ["backup", "incr"], ["backup", "full", "--first"]):
            with self.subTest(command=command):
                result = self.refused(*command)
                self.assertIn(f"disk gate: {command[0]} needs {FLOOR} free bytes on the filesystem of Docker's data root", result.stderr)
                self.assertIn(f"available {FLOOR - 1}", result.stderr)
                self.assertNotIn("the rehearsal root", result.stderr)
        self.free(docker=50 * GIB, root=FLOOR - 1, same=False)
        result = self.refused("up")
        self.assertIn(f"up needs {FLOOR} free bytes on the filesystem of the rehearsal root ({self.root}), available {FLOOR - 1}", result.stderr)
        self.free(docker=FLOOR, root=FLOOR, same=False)
        result = self.ok("up")
        self.assertIn(f"disk_gate step=up needed_bytes={FLOOR} available_bytes={FLOOR} floor_bytes={FLOOR}", result.stdout)
        self.assertEqual(result.stdout.count("disk_gate step=up"), 2)  # One fact per filesystem.
        self.free(docker=FLOOR - 1, root=FLOOR - 1, same=True)
        result = self.refused("up")
        self.assertIn("Docker's data root", result.stderr)
        self.assertIn("and the rehearsal root", result.stderr)  # One filesystem holds both.

    def test_the_floor_is_an_env_file_key_from_2_to_64(self):
        self.write_env(dict(t.VALUES, KOMIKA_REHEARSAL_MIN_FREE_GB="10"))
        self.free(docker=10 * GIB - 1, root=50 * GIB, same=False)
        self.assertIn(f"needs {10 * GIB} free bytes", self.refused("up").stderr)
        self.free(docker=10 * GIB, root=10 * GIB, same=False)
        self.ok("up")
        self.write_env(dict(t.VALUES, KOMIKA_REHEARSAL_MIN_FREE_GB="2"))
        self.free(docker=2 * GIB, root=2 * GIB)
        self.ok("up")
        for value in ("1", "65", "0", "99", "3.5", "3g", "-3", "03"):
            with self.subTest(value=value):
                self.write_env(dict(t.VALUES, KOMIKA_REHEARSAL_MIN_FREE_GB=value))
                self.refused("up")
                self.assertEqual(self.calls, [], "an invalid floor is refused before any Docker call")
        self.refused("up", KOMIKA_REHEARSAL_MIN_FREE_GB="2")  # Only the env file may set it.

    def test_import_adds_the_measured_growth_of_the_sqlite_file(self):
        sqlite, binary = self.base / "copy.sqlite3", self.base / "komika-server"
        sqlite.write_bytes(b"x" * 1000)
        binary.write_bytes(b"x")
        binary.chmod(0o755)
        job = ["--sqlite", str(sqlite), "--server-bin", str(binary)]
        need = FLOOR + 2200  # floor + 2.2 x 1000 bytes
        self.assertEqual(safety.IMPORT_GROWTH, 2.2)
        self.free(docker=need - 1, root=50 * GIB, same=False)
        result = self.refused("import", *job)
        self.assertIn(f"disk gate: import needs {need} free bytes on the filesystem of Docker's data root", result.stderr)
        self.assertIn(f"available {need - 1}", result.stderr)
        self.assertEqual(sorted(os.listdir(self.root)), [safety.ROOT_MARKER])  # No work directory was made.
        self.free(docker=need, root=FLOOR - 1, same=False)
        self.assertIn("the rehearsal root", self.refused("import", *job).stderr)
        self.free(docker=need, root=FLOOR, same=False)
        result = self.ok("import", *job)
        self.assertIn(f"disk_gate step=import needed_bytes={need} available_bytes={need}", result.stdout)
        # The verifier writes nothing to the data volume: no growth is added.
        self.free(docker=1, root=1)
        self.ok("verify", *job)

    def test_status_shows_free_bytes_and_the_floor_without_refusing(self):
        self.free(docker=7 * GIB, root=FLOOR - 5, same=False)
        result = self.ok("status")
        self.assertIn(f"watch-free docker_root={self.state['docker_root']} free_bytes={7 * GIB} floor_bytes={FLOOR} ok", result.stdout)
        self.assertIn(f"watch-free rehearsal_root={self.root} free_bytes={FLOOR - 5} floor_bytes={FLOOR} BELOW THE FLOOR", result.stdout)

    def test_preflight_reports_the_gate_and_refuses_below_the_floor(self):
        self.free(docker=9 * GIB, root=9 * GIB)
        result = self.ok("preflight")
        self.assertIn(f"disk_gate step=preflight needed_bytes={FLOOR} available_bytes={9 * GIB}", result.stdout)
        self.assertIn("disk_gate_rules=", result.stdout)
        self.free(docker=FLOOR - 1, root=9 * GIB, same=False)
        result = self.refused("preflight")
        self.assertIn("REFUSAL: disk gate: disk gate: preflight needs", result.stdout)

    def test_unmeasurable_docker_root_refuses_on_linux_only(self):
        self.rehome(self.base / "not-visible-here")
        with patch.object(safety, "PLATFORM", "darwin"):
            result = self.ok("up")
            self.assertIn("only the rehearsal root was measured", result.stdout)
        self.linux()
        self.refused("up")  # A local daemon whose data root is invisible is no local daemon.


class ImageIdentityTests(Harness):
    """F5: the image must carry exactly this kit's files."""

    def test_identity_is_one_fixed_command_before_compose_up(self):
        self.ok("up")
        checks = [call for call in self.calls if identity_call(call)]
        self.assertEqual(len(checks), 1)
        check = checks[0]
        self.assertEqual(check[:6], ["run", "--rm", "--pull=never", "--name", check[4], "--label"])
        self.assertTrue(re.fullmatch(re.escape(t.PROJECT) + "-meta-[0-9a-f]{8}", check[4]))
        self.assertEqual(check[6:], [
            f"{t.LABEL}={t.PROJECT}", "--network", "none", "--user", "65534:65534", "--read-only", "--cap-drop=ALL",
            "--security-opt=no-new-privileges", "--memory=64m", "--memory-swap=64m", "--cpus=0.5", "--pids-limit=32",
            "--log-driver=json-file", "--log-opt=max-size=10m", "--log-opt=max-file=3",  # C8: CPU and log bounds.
            "--entrypoint", "sha256sum", safety.IMAGE,
            *(f"/opt/komika-postgres/{name}" for name in ("entrypoint.sh", "pgbackrest.sh", "health.sh", "backup.sh",
                                                          "hosted.py", "safety.py", "postgresql.conf"))])
        up = [call for call in self.calls if call[0] == "compose" and "up" in call][0]
        self.assertLess(self.calls.index(check), self.calls.index(up))

    def test_any_differing_or_missing_file_refuses_every_start_from_the_image(self):
        sqlite, binary = self.base / "copy.sqlite3", self.base / "komika-server"
        sqlite.write_bytes(b"x")
        binary.write_bytes(b"x")
        binary.chmod(0o755)
        for name in safety.IMAGE_FILES:
            with self.subTest(differs=name):
                self.state["image_differs"] = {name: "# one byte more\n"}
                result = self.refused("up")
                self.assertIn(f"was not built from this kit: {name} differs", result.stderr)
                self.assertIn("rebuild the image", result.stderr)
        self.state["image_differs"] = {"safety.py": "x", "hosted.py": "y"}
        self.assertIn("hosted.py, safety.py differ from", self.refused("up").stderr)
        self.linux()
        self.state["containers"] = []
        for command in (["import", "--sqlite", str(sqlite), "--server-bin", str(binary)],
                        ["verify", "--sqlite", str(sqlite), "--server-bin", str(binary)],
                        ["info"], ["repo-destroy", "--yes-delete-rehearsal-repo"],
                        ["restore", "--target", str(self.root / "r"), "--json-out", str(self.root / "r.json")],
                        ["drill", "--target", str(self.root / "d"), "--json-out", str(self.root / "d.json")]):
            with self.subTest(command=command[0]):
                self.state["containers"] = [] if command[0] in ("info", "repo-destroy", "restore") else [t.container()]
                self.assertIn("was not built from this kit", self.refused(*command).stderr)
        self.state["image_differs"] = {}
        self.state["image_lacks"] = ["hosted.py"]
        self.assertIn("does not carry every kit file", self.refused("up").stderr)
        result = self.refused("preflight")
        self.assertIn("REFUSAL: image: ", result.stdout)

    def test_a_container_of_an_older_build_is_reported_after_up(self):
        self.state["containers"][0]["Image"] = "sha256:built-before-the-review"
        result = self.run_rehearsal("up")
        self.assertEqual(result.returncode, 1)
        self.assertIn("does not run the checked image build", result.stderr)

    def test_preflight_states_that_image_and_kit_are_identical(self):
        result = self.ok("preflight")
        self.assertIn("image_kit_files=identical to this kit (7 files: ", result.stdout)


class CleanupTests(Harness):
    """F2: nothing a rehearsal started may be left without a subcommand that removes it."""

    def setUp(self):
        super().setUp()
        self.sqlite = self.base / "copy.sqlite3"
        self.sqlite.write_bytes(b"SQLite format 3\0")
        self.binary = self.base / "komika-server"
        self.binary.write_bytes(b"\x7fELF")
        self.binary.chmod(0o755)
        self.job_container = labelled_container(t.PROJECT + "-job-0123abcd", "1")
        self.meta_container = labelled_container(t.PROJECT + "-meta-89abcdef", "2", running=False)

    def test_a_job_container_that_outlives_its_client_is_removed_in_the_same_run(self):
        # The fake `docker run` registers the container and returns like a killed client.
        self.state["run_lingers"] = 0.01
        self.state["run_status"] = 137
        result = self.run_rehearsal("import", "--sqlite", str(self.sqlite), "--server-bin", str(self.binary))
        self.assertEqual(result.returncode, 137)
        run = [call for call in self.mutating() if call[0] == "run"][0]
        name = run[run.index("--name") + 1]
        import hashlib
        self.assertEqual(self.mutating()[-1], ["rm", "-f", "-v", hashlib.sha256(name.encode()).hexdigest()])
        self.assertIn(f"job_container={name} was still present and has been removed", result.stdout)

    def test_sigterm_sighup_and_sigint_remove_the_running_job_container(self):
        import hashlib
        import signal
        import time
        self.state["run_lingers"] = 60

        def dispositions(ignored):
            # The test runner may itself have been started with signals ignored.
            def apply():
                for each in (signal.SIGINT, signal.SIGHUP, signal.SIGTERM):
                    signal.signal(each, signal.SIG_IGN if each in ignored else signal.SIG_DFL)
            return apply

        for number, status, ignored in ((signal.SIGTERM, 143, ()), (signal.SIGHUP, 129, ()), (signal.SIGINT, 130, ()),
                                        (signal.SIGTERM, 143, (signal.SIGHUP,))):
            with self.subTest(signal=number.name, ignored=[each.name for each in ignored]):
                self.state["containers"] = [t.container()]
                kit = self.prepare_process()
                process = subprocess.Popen(
                    ["sh", str(kit / "rehearsal.sh"), "import", "--sqlite", str(self.sqlite), "--server-bin", str(self.binary)],
                    env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, stdin=subprocess.DEVNULL,
                    preexec_fn=dispositions(ignored))
                self.addCleanup(process.kill)
                name = None
                deadline = time.monotonic() + 60
                while name is None and time.monotonic() < deadline:
                    time.sleep(0.05)
                    try:
                        present = [e["Name"].lstrip("/") for e in json.loads(self.state_file.read_text())["containers"]]
                    except ValueError:
                        continue  # The fake is writing its state at this moment.
                    name = next((n for n in present if "-job-" in n), None)
                self.assertIsNotNone(name, "the job container never started")
                if ignored:
                    # Under nohup a lost terminal does not end a long import.
                    process.send_signal(signal.SIGHUP)
                    time.sleep(1.5)
                    self.assertIsNone(process.poll(), "an ignored SIGHUP must stay ignored")
                process.send_signal(number)
                out, err = process.communicate(timeout=60)
                self.assertEqual(process.returncode, status, out + err)
                calls = [json.loads(line)["args"] for line in self.log.read_text().splitlines()]
                self.assertIn(["rm", "-f", "-v", hashlib.sha256(name.encode()).hexdigest()], calls)
                self.assertIn(f"job_container={name} was still present and has been removed", out)
                left = [e["Name"] for e in json.loads(self.state_file.read_text())["containers"]]
                self.assertEqual(left, ["/" + t.PROJECT + "-postgres"])

    def test_a_container_created_after_its_client_was_killed_is_still_removed(self):
        # The create request can reach the daemon while the client is already dead.
        import hashlib
        import signal
        import time
        self.state["run_late"] = 0.8
        kit = self.prepare_process()
        process = subprocess.Popen(
            ["sh", str(kit / "rehearsal.sh"), "import", "--sqlite", str(self.sqlite), "--server-bin", str(self.binary)],
            env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, stdin=subprocess.DEVNULL,
            preexec_fn=lambda: signal.signal(signal.SIGTERM, signal.SIG_DFL))
        self.addCleanup(process.kill)
        name = None
        deadline = time.monotonic() + 60
        while name is None and time.monotonic() < deadline:
            time.sleep(0.02)
            for line in (self.log.read_text().splitlines() if self.log.exists() else []):
                call = json.loads(line)["args"]
                if call[0] == "run" and "sha256sum" not in call:
                    name = call[call.index("--name") + 1]
        self.assertIsNotNone(name, "the job never reached docker run")
        process.send_signal(signal.SIGTERM)
        out, err = process.communicate(timeout=60)
        self.assertEqual(process.returncode, 143, out + err)
        time.sleep(1.2)  # Past the moment the late container would have appeared.
        calls = [json.loads(line)["args"] for line in self.log.read_text().splitlines()]
        self.assertIn(["rm", "-f", "-v", hashlib.sha256(name.encode()).hexdigest()], calls)
        left = [e["Name"] for e in json.loads(self.state_file.read_text())["containers"]]
        self.assertEqual(left, ["/" + t.PROJECT + "-postgres"])

    def test_status_lists_jobs_and_unknown_labelled_containers(self):
        self.state["containers"] += [self.job_container, self.meta_container,
                                     labelled_container("someone-elses-idea", "3")]
        result = self.ok("status")
        self.assertIn(f"job container {t.PROJECT}-job-0123abcd: state=running", result.stdout)
        self.assertIn(f"job container {t.PROJECT}-meta-89abcdef: state=exited", result.stdout)
        self.assertIn("UNKNOWN container someone-elses-idea: carries this rehearsal's label", result.stdout)
        self.assertFalse(any(call[0] in ("rm", "stop", "kill") for call in self.mutating()))

    def test_stop_source_and_down_remove_jobs_and_say_what_they_leave(self):
        target = self.root / "pitr"
        target.mkdir()
        restore = labelled_container("komika-pitr-0123456789ab", "9", **{safety.TARGET_LABEL: str(target)})
        restore["Mounts"] = [{"Type": "bind", "Source": str(target), "Destination": "/restore"}]
        near = [labelled_container(t.PROJECT + "-job-0123abcx", "4"), labelled_container(t.PROJECT + "-job-0123abcd-x", "5"),
                labelled_container("x-" + t.PROJECT + "-job-0123abcd", "6"), labelled_container("komika-server-1", "7")]
        other = labelled_container("komika-pg-rehearsal-other-job-0123abcd", "8")
        other["Config"]["Labels"][t.LABEL] = "komika-pg-rehearsal-other"
        self.state["containers"] += [self.job_container, self.meta_container, restore, other] + near
        result = self.run_rehearsal("stop-source")
        self.assertEqual(result.returncode, 1, self.output)
        self.assertEqual(self.mutating(), [["stop", "-t", "240", "a" * 64], ["rm", "a" * 64],
                                           ["rm", "-f", "-v", "1" * 64], ["rm", "-f", "-v", "2" * 64]])
        for entry in near:
            self.assertIn(f"REFUSED: container {entry['Name'].lstrip('/')}: carries this rehearsal's label but none of its name patterns",
                          result.stdout)
        self.assertNotIn("komika-pg-rehearsal-other", result.stdout)
        result = self.run_rehearsal("down")
        self.assertEqual(result.returncode, 1, self.output)
        self.assertEqual(self.mutating()[-1], ["network", "rm", self.state["networks"][0]["Id"]])
        self.assertEqual(len(self.mutating()), 5)
        self.assertIn(f"REFUSED: kept restore container komika-pitr-0123456789ab: not removed by down; run "
                      f"`rehearsal.sh restore-cleanup --target {target}`", result.stdout)
        self.assertEqual(result.stdout.count("REFUSED: "), 5)
        # Only its own objects: both succeed.
        self.state["containers"] = [t.container(), self.job_container]
        self.ok("stop-source")
        self.ok("down")
        self.assertEqual(self.mutating(), [["stop", "-t", "240", "a" * 64], ["rm", "a" * 64], ["rm", "-f", "-v", "1" * 64],
                                           ["network", "rm", self.state["networks"][0]["Id"]]])

    def test_down_keeps_a_network_that_a_foreign_container_uses_and_names_it(self):
        self.state["networks"][0]["Containers"] = {"c" * 64: {"Name": "komika-server-1"}}
        result = self.run_rehearsal("down")
        self.assertEqual(result.returncode, 1)
        self.assertIn(f"REFUSED: network {t.PROJECT}-network: still used by container(s) komika-server-1", result.stdout)
        self.assertFalse(any(call[:2] == ["network", "rm"] for call in self.mutating()))
        self.assertEqual(self.mutating(), [["stop", "-t", "240", "a" * 64], ["rm", "a" * 64]])


INFO = [{"name": "komika", "status": {"code": 0}, "repo": [{"status": {"code": 0}}],
         "archive": [{"min": "000000010000000000000001", "max": "000000010000000000000009"}],
         "backup": [{"label": "20261011-120000F", "type": "full", "timestamp": {"start": 1791720000, "stop": 1791720100},
                     "info": {"size": 1000, "repository": {"delta": 300}}},
                    {"label": "20261011-120000F_20261011-130000I", "type": "incr",
                     "timestamp": {"start": 1791723600, "stop": 1791723700}, "info": {"size": 2000, "repository": {"delta": 50}}}]}]


class RestoreRunTests(Harness):
    """F4 and F11: `restore` runs to its end against the fake Docker, on every platform."""

    def setUp(self):
        super().setUp()
        self.linux()
        import datetime
        self.time = datetime.datetime.fromtimestamp(1791724000, datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.000000Z")
        # The source is gone: no rehearsal container, no rehearsal volume; the network is still there.
        self.state["containers"] = []
        self.state["volumes"] = self.state["volumes"][2:]
        self.state["run"] = {"info": json.dumps(INFO), "repo-ls": json.dumps({"backup/komika/backup.info": {"type": "file", "size": 77}})}
        self.state["exec"] = {"pg_controldata": CONTROL, "SELECT NOT pg_is_in_recovery": "t", "SELECT current_user": "komika",
                              "FROM pg_database": "1", "FROM operator_markers": "before-pitr",
                              "FROM work": "work=1\nchapter=2\nusers=3", "du -sb": "12345\t/restore"}
        self.settings = self.root / "source-settings.json"
        self.settings.write_text(json.dumps({"pg_controldata": CONTROL, "settings": {
            "max_connections": 48, "max_worker_processes": 8, "max_wal_senders": 10,
            "max_prepared_transactions": 0, "max_locks_per_transaction": 64}}))

    def arguments(self, name="pitr", *extra):
        return ["restore", "--target", str(self.root / name), "--json-out", str(self.root / (name + ".json")),
                "--target-time", self.time, "--source-settings", str(self.settings), *extra]

    def check_success(self, name, result):
        target = self.root / name
        report = json.loads((self.root / (name + ".json")).read_text())
        self.assertEqual(report["operator_markers"], ["before-pitr"])
        self.assertEqual(report["backup_label"], "20261011-120000F_20261011-130000I")  # The last backup before T.
        self.assertEqual(report["restored_directory"], str(target))
        self.assertEqual(json.loads(result.stdout.splitlines()[-1]), report)
        self.assertIn("operator_markers=before-pitr", result.stdout)
        self.assertIn("marker expectations met: present=before-pitr absent=after-pitr", result.stdout)
        need = FLOOR + 2600  # floor + 1.3 x the selected backup's database size
        self.assertIn(f"disk_gate step=restore needed_bytes={need}", result.stdout)
        self.assertTrue(target.is_dir())
        work = self.mutating()
        readers = [call for call in work if call[0] == "run"]
        self.assertEqual([call[-2:] for call in readers], [["--output=json", "info"], ["--output=json", "repo-ls"]])
        for call in readers:
            self.assertIn(f"--network={t.PROJECT}-network", call)
            self.assertFalse(any(arg.startswith(("--mount", "--volume")) or arg == "-v" for arg in call))
        self.assertFalse(any("bridge" in arg and arg != "bridge" and not arg.startswith("komika-pitr-") for call in work for arg in call
                             if arg.startswith("--network")), "no container joins Docker's default bridge")
        created = [call for call in work if call[0] == "create"][0]
        name_of = created[created.index("--name") + 1]
        self.assertTrue(re.fullmatch("komika-pitr-[0-9a-f]{12}", name_of))
        network_create = [call for call in work if call[:2] == ["network", "create"]][0]
        self.assertEqual(network_create[-4:], ["bridge", "--subnet", "172.29.42.0/24", name_of + "-network"])
        self.assertEqual(created[created.index("--network") + 1], name_of + "-network")
        for expected in (f"{t.LABEL}={t.PROJECT}", f"{safety.TARGET_LABEL}={target}", "--memory=3g", "--memory-swap=3g",
                         "--cpus=1", "--pids-limit=128", f"type=bind,src={target},dst=/restore", safety.IMAGE):
            self.assertIn(expected, created)
        record = [r for r in self.records if r["args"][0] == "create"][0]
        self.assertEqual(record["stdin_values"]["KOMIKA_R2_PREFIX"], "rehearsal/test")
        self.assertEqual(record["stdin_values"]["KOMIKA_PG_ALLOWED_CIDRS"], "172.29.42.0/24")
        self.assertTrue(record["stdin_has_password"])
        restore = [call for call in work if call[0] == "exec" and call[-1] == "restore"][0]
        self.assertEqual(restore[4:], ["/opt/komika-postgres/pgbackrest.sh", "--log-level-stderr=detail",
                                       "--set=20261011-120000F_20261011-130000I", "--type=time",
                                       # PostgreSQL refuses `Z` here: UTC with a numeric offset is sent.
                                       "--target=" + self.time.replace("T", " ").replace("Z", "+00"),
                                       "--target-action=promote", "restore"])
        # Every pgBackRest command line the kit sent is on the in-image allowlist.
        sent = [call[call.index("/opt/komika-postgres/pgbackrest.sh") + 1:] for call in work
                if "/opt/komika-postgres/pgbackrest.sh" in call]
        self.assertEqual(len(sent), 3)
        for arguments in sent:
            safety.repository_arguments(arguments, {"KOMIKA_REHEARSAL_PROJECT": t.PROJECT})
        return name_of

    def test_restore_reaches_its_end_and_the_kit_checks_the_markers(self):
        expectations = ["--expect-marker", "before-pitr", "--expect-no-marker", "after-pitr"]
        result = self.ok(*self.arguments("pitr", "--keep", *expectations))
        name = self.check_success("pitr", result)
        self.assertIn(f"throwaway_container={name}", result.stdout)
        self.assertFalse(any(call[0] == "rm" or call[:2] == ["network", "rm"] for call in self.mutating()))  # --keep
        # Without --keep the server is stopped and exactly its container and network are removed.
        result = self.ok(*self.arguments("second", *expectations))
        name = self.check_success("second", result)
        import hashlib
        self.assertEqual(self.mutating()[-2:], [["rm", "-f", "-v", name],
                                                ["network", "rm", name + "-network"]])
        self.assertTrue(any(call[-6:] == ["pg_ctl", "-D", "/restore", "-m", "fast", "-w", "stop"][-6:] for call in self.mutating()))
        if sys.platform.startswith("linux"):  # And through the real shell entry point on the target.
            result = self.ok(*self.arguments("shell", *expectations), shell=True)
            self.check_success("shell", result)

    def test_a_restore_to_the_wrong_time_fails_the_marker_expectation(self):
        self.state["exec"]["FROM operator_markers"] = "before-pitr\nafter-pitr"
        result = self.run_rehearsal(*self.arguments("late", "--keep", "--expect-marker", "before-pitr", "--expect-no-marker", "after-pitr"))
        self.assertEqual(result.returncode, 1, self.output)
        self.assertIn("marker expectation failed: restored operator_markers=before-pitr,after-pitr; "
                      "must be absent but present: after-pitr", result.stderr)
        self.assertFalse((self.root / "late.json").exists())  # No report of a failed check.
        # The directory stays; the operator is told where it is, who owns it and how to remove it.
        self.assertIn(f"The directory {self.root / 'late'} stays and now belongs to the image's postgres user (uid 999)", result.stderr)
        self.assertIn(f"sudo rm -rf -- {self.root / 'late'}", result.stderr)
        # Even with --keep nothing of a failed restore keeps running.
        removed = [call for call in self.mutating() if call[0] == "rm" or call[:2] == ["network", "rm"]]
        self.assertEqual([call[:2] for call in removed], [["rm", "-f"], ["network", "rm"]])
        for markers, expect, message in (("", ["--expect-marker", "before-pitr"], "expected but missing: before-pitr"),
                                         ("after-pitr", ["--expect-marker", "before-pitr", "--expect-no-marker", "after-pitr"],
                                          "expected but missing: before-pitr; must be absent but present: after-pitr"),
                                         ("a\nb", ["--expect-marker", "a", "--expect-marker", "c"], "expected but missing: c")):
            with self.subTest(markers=markers):
                self.state["exec"]["FROM operator_markers"] = markers
                name = "case" + str(len(os.listdir(self.root)))
                result = self.run_rehearsal(*self.arguments(name, *expect))
                self.assertEqual(result.returncode, 1, self.output)
                self.assertIn(message, result.stderr)
        # No marker database at all: an expectation cannot be met.
        self.state["exec"]["FROM pg_database"] = "0"
        result = self.run_rehearsal(*self.arguments("nodb", "--expect-marker", "before-pitr"))
        self.assertEqual(result.returncode, 1)
        self.assertIn("restored operator_markers=(none)", result.stderr)
        # Without expectations the markers are only reported.
        result = self.ok(*self.arguments("plain"))
        self.assertEqual(json.loads((self.root / "plain.json").read_text())["operator_markers"], [])

    def test_the_shell_entry_point_restores_on_every_platform(self):
        """F11: `rehearsal.sh restore` itself, as a process, runs to success on every
        development platform. The platform and the mount table are the two module
        values of safety.py; a private copy of the kit carries the injected ones,
        because nothing in the environment or on a command line can set them."""
        kit = self.base / "kit"
        shutil.copytree(safety.KIT, kit, ignore=shutil.ignore_patterns("__pycache__", "test_*"))
        text = (kit / "safety.py").read_text()
        for old, new in (("PLATFORM = sys.platform\n", 'PLATFORM = "linux"\n'),
                         ('    return Path("/proc/self/mountinfo").read_text()\n', f"    return {MOUNTINFO!r}\n")):
            self.assertEqual(text.count(old), 1)
            text = text.replace(old, new)
        (kit / "safety.py").write_text(text)
        self.kit = kit
        expectations = ["--expect-marker", "before-pitr", "--expect-no-marker", "after-pitr"]
        result = self.ok(*self.arguments("shell", "--keep", *expectations))
        self.assertIn(str(kit / "rehearsal-compose.yml"), json.dumps(self.calls))  # The copy ran, as a process.
        name = self.check_success("shell", result)
        self.assertIn(f"throwaway_container={name}", result.stdout)
        # The same process refuses on the platform it really runs on when that is not Linux.
        if not sys.platform.startswith("linux"):
            self.kit = safety.KIT
            result = self.refused(*self.arguments("native"), shell=True)
            self.assertIn("Linux Docker host", result.stderr)

    def test_a_target_beyond_the_archived_wal_fails_at_once_with_the_servers_reason(self):
        # Found by the first wrong-time restore against a repository: PostgreSQL shut
        # down ("recovery ended before configured recovery target was reached") and
        # the kit polled the dead server for 1,800 seconds.
        import time
        self.state["exec_status"] = {"SELECT NOT pg_is_in_recovery": 2, "-D /restore status": 3}
        self.state["exec"]["tail -n 60"] = ("2026-10-11 15:00:32.643 GMT [79] FATAL:  recovery ended before configured "
                                            "recovery target was reached\n2026-10-11 15:00:32.651 GMT [73] LOG:  database system is shut down")
        before = time.monotonic()
        result = self.run_rehearsal(*self.arguments("beyond", "--keep", "--expect-marker", "before-pitr"))
        self.assertEqual(result.returncode, 1, self.output)
        self.assertLess(time.monotonic() - before, 60)
        self.assertIn("rehearsal refused: the restored server stopped during recovery: ", result.stderr)
        self.assertIn("FATAL:  recovery ended before configured recovery target was reached", result.stderr)
        self.assertIn("choose an earlier --target-time", result.stderr)
        self.assertIn(f"The directory {self.root / 'beyond'} stays", result.stderr)
        self.assertFalse((self.root / "beyond.json").exists())
        self.assertEqual([call[:2] for call in self.mutating()[-2:]], [["rm", "-f"], ["network", "rm"]])
        # A server that is merely still starting is waited for: status 0, then an answer.
        self.state["exec_status"] = {"-D /restore status": 0}
        self.ok(*self.arguments("patient"))

    def test_a_failed_restore_names_the_directory_and_its_owner(self):
        self.state["exec_fails"] = "pg_ctl"
        result = self.run_rehearsal(*self.arguments("broken"))
        self.assertEqual(result.returncode, 1)
        self.assertIn(f"The directory {self.root / 'broken'} stays", result.stderr)
        self.assertIn("uid 999", result.stderr)
        self.assertTrue((self.root / "broken").is_dir())
        self.assertEqual([call[:2] for call in self.mutating()[-2:]], [["rm", "-f"], ["network", "rm"]])
        # A refusal before the directory exists says nothing about a directory.
        result = self.refused(*self.arguments("early", "--expect-marker", "bad label"))
        self.assertNotIn("stays", result.stderr)

    def test_restore_needs_the_backup_size_on_the_target_filesystem(self):
        need = FLOOR + 2600
        self.assertEqual(safety.RESTORE_GROWTH, 1.3)
        self.free(docker=50 * GIB, root=need - 1, same=False)
        result = self.run_rehearsal(*self.arguments("r1"))
        self.assertEqual(result.returncode, 1)
        self.assertIn(f"disk gate: restore needs {need} free bytes on the filesystem of the rehearsal root ({self.root}), "
                      f"available {need - 1}", result.stderr)
        self.assertNotIn("stop-source", result.stderr)  # The source is already gone: no such advice.
        self.assertFalse((self.root / "r1").exists())
        self.assertFalse(any(call[0] == "create" or call[:2] == ["network", "create"] for call in self.mutating()))
        # The source volumes still exist: the refusal names the order that fits.
        self.state["volumes"] += [volume(t.PROJECT + "-postgres-data"), volume(t.PROJECT + "-postgres-socket")]
        result = self.run_rehearsal(*self.arguments("r2"))
        self.assertIn("stop-source, destroy-source-volumes --yes-delete-rehearsal-volumes, then restore --target-time", result.stderr)
        # An earlier target time selects the smaller full backup: its size counts.
        import datetime
        early = datetime.datetime.fromtimestamp(1791720200, datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        self.free(docker=50 * GIB, root=FLOOR + 1300, same=False)
        result = self.ok("restore", "--target", str(self.root / "r3"), "--json-out", str(self.root / "r3.json"), "--target-time", early)
        self.assertIn(f"disk_gate step=restore needed_bytes={FLOOR + 1300}", result.stdout)
        self.assertIn("--set=20261011-120000F", json.dumps(self.mutating()))
        # A backup without a database size cannot be gated: refuse rather than guess.
        info = json.loads(json.dumps(INFO))
        del info[0]["backup"][1]["info"]["size"]
        self.state["run"]["info"] = json.dumps(info)
        result = self.run_rehearsal(*self.arguments("r4"))
        self.assertEqual(result.returncode, 1)
        self.assertIn("no database size", result.stderr)

    def test_drill_includes_the_restore_need_while_the_source_exists(self):
        self.state["containers"] = [t.container()]
        self.state["volumes"] += [volume(t.PROJECT + "-postgres-data"), volume(t.PROJECT + "-postgres-socket")]
        self.state["exec"].update({"SHOW ": "48", "--output=json info": json.dumps(INFO)})
        need = FLOOR + 2600
        self.free(docker=need - 1, root=need - 1, same=True)
        result = self.run_rehearsal("drill", "--target", str(self.root / "d"), "--json-out", str(self.root / "d.json"))
        self.assertEqual(result.returncode, 1, self.output)
        self.assertIn(f"disk gate: drill needs {need} free bytes", result.stderr)
        self.assertIn(f"available {need - 1}", result.stderr)
        self.assertIn("the order that needs the least space is stop-source, destroy-source-volumes "
                      "--yes-delete-rehearsal-volumes, then restore --target-time", result.stderr)
        self.assertFalse((self.root / "d").exists())
        touched = json.dumps(self.mutating())
        for forbidden in ("INSERT INTO", "pg_switch_wal", "network\", \"create", "\"create\""):
            self.assertNotIn(forbidden, touched)


class LocalStoreTests(Harness):
    """Part B: KOMIKA_R2_VERIFY_TLS=0 exists for a `*-local` project and a private literal address only."""
    LOCAL = t.PROJECT + "-local"

    def local(self, **values):
        """Select a `*-local` project with a private literal endpoint."""
        chosen = dict(t.VALUES, KOMIKA_REHEARSAL_PROJECT=self.LOCAL, KOMIKA_R2_ENDPOINT="10.213.4.1",
                      KOMIKA_R2_VERIFY_TLS="0")
        chosen.update(values)
        self.write_env({key: value for key, value in chosen.items() if value is not None})
        self.env["KOMIKA_REHEARSAL_PROJECT"] = self.LOCAL
        self.state["config"] = json.loads(json.dumps(t.rendered()).replace(t.PROJECT, self.LOCAL))
        for service in self.state["config"]["services"].values():
            service["environment"].update(KOMIKA_R2_ENDPOINT=chosen["KOMIKA_R2_ENDPOINT"],
                                          KOMIKA_R2_VERIFY_TLS="0" if chosen.get("KOMIKA_R2_VERIFY_TLS") == "0" else "1")
        self.state["containers"] = json.loads(json.dumps([t.container()]).replace(t.PROJECT, self.LOCAL))
        self.state["containers"][0]["Config"]["Env"] = [
            f"{key}={value}" for key, value in self.state["config"]["services"]["postgres"]["environment"].items()]
        self.state["volumes"] = self.state["volumes"][2:]
        self.state["networks"] = [network(self.LOCAL + "-network", "172.30.21.0/24", **{t.LABEL: self.LOCAL})]

    def test_accepted_only_with_a_local_project_and_a_private_literal_address(self):
        for endpoint in ("10.213.4.1", "172.16.0.1", "172.31.255.254", "192.168.1.1", "https://192.168.1.1"):
            with self.subTest(endpoint=endpoint):
                self.local(KOMIKA_R2_ENDPOINT=endpoint)
                self.ok("up")
        # The rendering must carry the same decision: an env file without the key renders 1.
        self.local()
        for service in self.state["config"]["services"].values():
            service["environment"]["KOMIKA_R2_VERIFY_TLS"] = "1"
        self.assertIn("rehearsal topology", self.refused("up").stderr)

    def test_refused_everywhere_else_before_any_docker_call(self):
        cases = {
            "host name": dict(KOMIKA_R2_ENDPOINT="minio.internal"),
            "real R2 endpoint": dict(KOMIKA_R2_ENDPOINT="https://0123456789abcdef.r2.cloudflarestorage.com"),
            "public address": dict(KOMIKA_R2_ENDPOINT="8.8.8.8"),
            "just outside 172.16/12": dict(KOMIKA_R2_ENDPOINT="172.32.0.1"),
            "loopback": dict(KOMIKA_R2_ENDPOINT="127.0.0.1"),
            "link local": dict(KOMIKA_R2_ENDPOINT="169.254.169.254"),
            "carrier grade NAT": dict(KOMIKA_R2_ENDPOINT="100.64.0.1"),
            "leading zero": dict(KOMIKA_R2_ENDPOINT="010.1.1.1"),
            "three octets": dict(KOMIKA_R2_ENDPOINT="10.1.1"),
            "octet out of range": dict(KOMIKA_R2_ENDPOINT="10.1.1.256"),
            "port": dict(KOMIKA_R2_ENDPOINT="10.1.1.1:9000"),
            "value 1": dict(KOMIKA_R2_VERIFY_TLS="1"),
            "value false": dict(KOMIKA_R2_VERIFY_TLS="false"),
            "value n": dict(KOMIKA_R2_VERIFY_TLS="n"),
        }
        for name, change in cases.items():
            with self.subTest(case=name):
                self.local(**change)
                for command in (["up"], ["backup", "incr"], ["repo-destroy", "--yes-delete-rehearsal-repo"], ["preflight"]):
                    self.refused(*command)
                    if command != ["preflight"]:
                        self.assertEqual(self.calls, [], "refused before any Docker call")
        # A project that is not named *-local: the same file is refused.
        self.env["KOMIKA_REHEARSAL_PROJECT"] = t.PROJECT
        self.write_env(dict(t.VALUES, KOMIKA_R2_ENDPOINT="10.213.4.1", KOMIKA_R2_VERIFY_TLS="0"))
        result = self.refused("up")
        self.assertIn("ends with -local", result.stderr)
        self.assertEqual(self.calls, [])
        for project in ("komika-pg-rehearsal-local-x", "komika-pg-rehearsal-xlocal", "komika-pg-rehearsal-phase6-r1"):
            with self.subTest(project=project):
                self.write_env(dict(t.VALUES, KOMIKA_REHEARSAL_PROJECT=project, KOMIKA_R2_ENDPOINT="10.213.4.1",
                                    KOMIKA_R2_VERIFY_TLS="0"))
                self.refused("up", KOMIKA_REHEARSAL_PROJECT=project)
                self.assertEqual(self.calls, [])
        # Never from the process environment, and never together with NO_REPO.
        self.local(KOMIKA_R2_VERIFY_TLS=None)
        self.refused("up", KOMIKA_R2_VERIFY_TLS="0")
        self.assertEqual(self.calls, [])
        self.write_env(dict(t.NO_REPO_VALUES, KOMIKA_REHEARSAL_PROJECT=self.LOCAL, KOMIKA_R2_VERIFY_TLS="0"))
        self.refused("up")
        self.assertEqual(self.calls, [])

    def test_the_decision_reaches_every_container_that_talks_to_the_repository(self):
        self.linux()
        self.local()
        self.state["containers"] = []
        self.state["run"] = {"info": json.dumps(INFO), "repo-ls": "{}"}
        self.state["exec"] = {"pg_controldata": CONTROL, "SELECT NOT pg_is_in_recovery": "t", "SELECT current_user": "komika",
                              "FROM pg_database": "0", "FROM work": "work=1", "du -sb": "1\t/restore"}
        self.ok("restore", "--target", str(self.root / "r"), "--json-out", str(self.root / "r.json"))
        sent = [record["stdin_values"] for record in self.records
                if record["args"][0] in ("run", "create") and not identity_call(record["args"])]
        self.assertEqual(len(sent), 3)
        for values in sent:
            self.assertEqual(values["KOMIKA_R2_VERIFY_TLS"], "0")
            self.assertEqual(values["KOMIKA_R2_ENDPOINT"], "10.213.4.1")
            self.assertEqual(values["KOMIKA_REHEARSAL_PROJECT"], self.LOCAL)


PGBACKREST = "/opt/komika-postgres/pgbackrest.sh"
BOUND = {"type": "none", "o": "bind", "device": "/srv/komika/data"}
LISTING = {
    "archive": {"type": "path"}, "archive/komika": {"type": "path"},
    "archive/komika/archive.info": {"type": "file", "size": 100},
    "archive/komika/18-1/0000000100000000/000000010000000000000001-" + "ab" * 20 + ".zst": {"type": "file", "size": 6 << 20},
    "backup/komika/backup.info": {"type": "file", "size": 77},
    "backup/komika/20261011-120000F/pg_data/base/5/16384.zst": {"type": "file", "size": 5 << 20},
}
USAGE = dict(objects=4, bytes=100 + (6 << 20) + 77 + (5 << 20), archive_objects=2, archive_bytes=100 + (6 << 20),
             backup_objects=2, backup_bytes=77 + (5 << 20), wal_segments=1, multipart_objects=1,
             write_requests_floor=1 + (2 + 2) + 1 + 1)


class JobFiles(Harness):
    def setUp(self):
        super().setUp()
        self.sqlite = self.base / "copy.sqlite3"
        self.sqlite.write_bytes(b"SQLite format 3\0")
        self.binary = self.base / "komika-server"
        self.binary.write_bytes(b"\x7fELF")
        self.binary.chmod(0o755)
        self.job = ["--sqlite", str(self.sqlite), "--server-bin", str(self.binary)]

    def sync(self):
        """Take over what the fake daemon holds now (another process changed it)."""
        self.state = json.loads(self.state_file.read_text())

    def ledger(self):
        return [entry["name"] for entry in safety.read_ledger(self.lock_path)[1]]


class StorageTests(JobFiles):
    """C1: a rehearsal label is no proof of rehearsal storage."""

    def commands(self):
        return (["up"], ["status"], ["psql-check"], ["backup", "full", "--first"], ["stanza-create"], ["check"], ["info"],
                ["repo-usage"], ["archive-status"], ["sql-marker", "--label", "a"], ["import", *self.job],
                ["verify", *self.job], ["destroy-source-volumes", "--yes-delete-rehearsal-volumes"],
                ["repo-destroy", "--yes-delete-rehearsal-repo"], ["record-settings", "--out", str(self.root / "s.json")],
                ["restore", "--target", str(self.root / "r"), "--json-out", str(self.root / "r.json")])

    def volumes(self, **change):
        """Both source volumes with the right name and label; the data volume changed."""
        return [dict(volume(t.PROJECT + "-postgres-data"), **change), volume(t.PROJECT + "-postgres-socket"),
                volume("komika_server-data", labelled=False)]

    def test_a_labelled_volume_bound_to_a_production_directory_is_refused_before_compose_and_every_use(self):
        """The checker's first accepted state, through the entry point."""
        changes = {"bind options": dict(Options=BOUND), "another driver": dict(Driver="nfs"),
                   "mountpoint elsewhere": dict(Mountpoint="/srv/komika/data")}
        for index, (name, change) in enumerate(changes.items()):
            for containers in ([], [t.container()]):  # Created ahead of `up`, or already in use.
                commands = self.commands()
                for command in (commands if index == 0 else commands[index::5]):
                    with self.subTest(change=name, containers=len(containers), command=command[0]):
                        self.state["containers"] = containers
                        self.state["volumes"] = self.volumes(**change)
                        result = self.refused(*command)
                        self.assertIn(f"volume {t.PROJECT}-postgres-data is not rehearsal storage", result.stderr)
                        self.assertIn("Its label does not prove what it is backed by", result.stderr)
                        self.assertNotIn("/srv/komika/data", json.dumps(self.calls))
                        # Not even the image check ran: nothing was created for a refused selection.
                        self.assertFalse(any(identity_call(call) or call[0] == "compose" and "up" in call
                                             for call in self.calls))
        self.state["containers"] = []
        self.state["volumes"] = self.volumes(Options=BOUND)
        result = self.refused("preflight")
        self.assertIn(f"REFUSAL: source storage: volume {t.PROJECT}-postgres-data is not rehearsal storage: it has driver options",
                      result.stdout)
        self.assertNotIn("source_storage=", result.stdout)
        # The control: the same volumes without the change are used.
        self.state["volumes"] = self.volumes()
        self.state["up_creates"] = {"containers": [t.container()]}
        self.ok("up")
        self.assertIn("source_storage=existing source volumes and service containers are plain local rehearsal storage",
                      self.ok("preflight").stdout)

    def test_a_service_container_whose_mount_source_changed_is_refused_before_any_exec(self):
        """The checker's second accepted state: volume name and destination kept."""
        for service in safety.SERVICES:
            for source in ("/srv/komika/data", PRODUCTION_VOLUME):
                for command in self.commands()[1::3] + (["logs"],):
                    with self.subTest(service=service, source=source, command=command[0]):
                        changed = t.container(service)
                        changed["Mounts"][0]["Source"] = source
                        self.state["containers"] = [changed] if service == "postgres" else [t.container(), changed]
                        result = self.refused(*command)
                        self.assertIn(f"container {t.PROJECT}-{service} is not rehearsal storage: its mounts are not exactly "
                                      "the two rehearsal volumes with their own source directories", result.stderr)

    def test_a_host_alias_and_a_foreign_user_of_the_volume_are_refused(self):
        path = f"{self.state['docker_root']}/volumes/{t.PROJECT}-postgres-data/_data"
        production = dict(t.container(), Name="/komika-server-1", Id="d" * 64)
        production["Config"] = dict(production["Config"], Labels={"com.docker.compose.project": "komika"})
        cases = {
            "by name": [{"Type": "volume", "Name": t.PROJECT + "-postgres-data", "Source": path, "Destination": "/data",
                         "Driver": "local"}],
            "by path": [{"Type": "bind", "Source": path, "Destination": "/data"}],
            "the volumes directory": [{"Type": "bind", "Source": self.state["docker_root"] + "/volumes", "Destination": "/v"}],
        }
        for name, mounts in cases.items():
            for command in (["up"], ["backup", "incr"], ["import", *self.job]):
                with self.subTest(case=name, command=command[0]):
                    self.state["containers"] = [t.container(), dict(production, Mounts=mounts)]
                    result = self.refused(*command)
                    self.assertIn("is not rehearsal storage", result.stderr)
                    self.assertIn("komika-server-1", result.stderr)
        # On the Linux target the host mount table is asked too: a directory bound onto the volume's own.
        self.state["containers"] = [t.container()]
        self.linux()
        with patch.object(safety, "read_mountinfo", lambda: MOUNTINFO + f"41 25 8:1 /srv/komika/data {path} rw - ext4 /dev/sda1 rw\n"):
            for command in (["up"], ["psql-check"], ["destroy-source-volumes", "--yes-delete-rehearsal-volumes"]):
                with self.subTest(case="mount table", command=command[0]):
                    result = self.refused(*command)
                    self.assertIn(f"is mounted at {path}", result.stderr)
        self.ok("psql-check")  # Without the alias the same state is used.

    def test_stop_source_and_down_still_remove_the_containers_and_never_the_storage(self):
        for command in ("stop-source", "down"):
            with self.subTest(command=command):
                self.state["containers"] = [t.container(), t.container("postgres-backup")]
                self.state["volumes"] = self.volumes(Options=BOUND)
                result = self.run_rehearsal(command)
                self.assertEqual(result.returncode, 1, self.output)
                self.assertIn(f"REFUSED: volume {t.PROJECT}-postgres-data is not rehearsal storage: it has driver options",
                              result.stdout)
                self.assertEqual(self.mutating()[:4], [["stop", "-t", "240", "b" * 64], ["rm", "b" * 64],
                                                       ["stop", "-t", "240", "a" * 64], ["rm", "a" * 64]])
                self.assertFalse(any(call[0] == "volume" or "-v" in call for call in self.mutating()))

    def test_volumes_that_compose_has_just_created_are_judged_too(self):
        self.absent_project()
        self.state["networks"].append(network(t.PROJECT + "-network", "172.30.21.0/24"))
        self.state["run"] = {"info": "[]"}
        self.state["up_creates"] = {"containers": [t.container()], "volumes": self.volumes(Options=BOUND)[:2]}
        result = self.run_rehearsal("up")
        self.assertEqual(result.returncode, 1, self.output)
        self.assertIn("is not rehearsal storage: it has driver options", result.stderr)
        self.assertFalse(any(call[0] in ("exec", "logs") for call in self.calls))

    def test_init_root_needs_a_project_of_which_nothing_exists(self):
        """`preflight --init-root` starts a NEW project: every name of it must be absent."""
        stranger = dict(t.container(), Name="/unexpected", Id="e" * 64)
        unlabelled = t.container()
        unlabelled["Config"] = dict(unlabelled["Config"], Labels={})
        composed = dict(t.container(), Name="/somebody-else", Id="9" * 64)
        composed["Config"] = dict(composed["Config"], Labels={"com.docker.compose.project": t.PROJECT})
        cases = {
            "postgres container": {"containers": [t.container()]},
            "backup container": {"containers": [t.container("postgres-backup")]},
            "a container with the project's name and no label": {"containers": [unlabelled]},
            "job container": {"containers": [labelled_container(t.PROJECT + "-job-0123abcd", "1")]},
            "metadata container": {"containers": [labelled_container(t.PROJECT + "-meta-89abcdef", "2", running=False)]},
            "kept restore": {"containers": [labelled_container("komika-pitr-0123456789ab", "3",
                                                               **{safety.TARGET_LABEL: str(self.root / "pitr")})]},
            "labelled stranger": {"containers": [stranger]},
            "Compose project label only": {"containers": [composed]},
            "data volume": {"volumes": [volume(t.PROJECT + "-postgres-data")]},
            "socket volume": {"volumes": [volume(t.PROJECT + "-postgres-socket")]},
            "unlabelled data volume": {"volumes": [volume(t.PROJECT + "-postgres-data", labelled=False)]},
            "labelled volume of another name": {"volumes": [volume("old-rehearsal-data")]},
            "network": {"networks": [network(t.PROJECT + "-network", "172.30.21.0/24")]},
            "kept restore network": {"networks": [network("komika-pitr-0123456789ab-network", "172.29.42.0/24")]},
        }
        new = self.base / "fresh-root"
        self.absent_project()
        nothing = json.dumps(self.state)
        for name, change in cases.items():
            with self.subTest(existing=name):
                self.state = json.loads(nothing)
                for kind, entries in change.items():
                    self.state[kind] = self.state[kind] + entries
                result = self.refused("preflight", "--init-root", KOMIKA_REHEARSAL_ROOT=str(new))
                self.assertIn("REFUSAL: new project: --init-root starts a NEW rehearsal project, and resources of "
                              + t.PROJECT + " already exist: ", result.stdout)
                self.assertFalse(os.path.lexists(new), "the root of a project that is not new was created")
                self.assertNotIn("rehearsal_root_created", result.stdout)
                # Nothing at all was created: not even the image check's short container.
                self.assertFalse(any(call[0] in ("run", "create") for call in self.calls))
        # Production's and other projects' resources do not count.
        self.state = json.loads(nothing)
        self.state["containers"].append(dict(stranger, Mounts=[], Config=dict(stranger["Config"], Labels={
            "com.docker.compose.project": "komika", t.LABEL: "komika-pg-rehearsal-other"})))
        self.state["volumes"].append(dict(volume("komika-pg-rehearsal-other-postgres-data"),
                                          Labels={t.LABEL: "komika-pg-rehearsal-other"}))
        result = self.ok("preflight", "--init-root", KOMIKA_REHEARSAL_ROOT=str(new))
        self.assertIn("rehearsal_root_created=yes", result.stdout)
        # Without --init-root an existing project is what preflight expects.
        self.state["containers"].append(t.container())
        self.ok("preflight", KOMIKA_REHEARSAL_ROOT=str(new))


class ImplicitRestoreTests(Harness):
    """C3: in a rehearsal `up` starts or initializes; it never restores."""

    def setUp(self):
        super().setUp()
        self.absent_project()
        self.state["networks"].append(network(t.PROJECT + "-network", "172.30.21.0/24"))
        self.state["run"] = {"info": json.dumps(INFO)}  # The repository under the rehearsal prefix holds two backups.
        self.created = [volume(t.PROJECT + "-postgres-data"), volume(t.PROJECT + "-postgres-socket")]
        self.state["up_creates"] = {"containers": [t.container()], "volumes": self.created}

    def ups(self):
        return [call for call in self.calls if call[0] == "compose" and "up" in call]

    def readers(self):
        return [call for call in self.calls if call[0] == "run" and not identity_call(call)]

    def test_absent_volumes_and_a_repository_with_a_backup_refuse_up_before_compose(self):
        # Exactly the floor is free: the disk gate alone admits `up`, and the restore
        # that the image would then run is larger than everything that is left.
        self.free(docker=FLOOR, root=FLOOR, same=False)
        for arguments, shell in ((["up"], False), (["up", "--with-scheduler"], False), (["up"], True)):
            with self.subTest(arguments=arguments, shell=shell):
                result = self.run_rehearsal(*arguments, shell=shell)
                self.assertEqual(result.returncode, 1, self.output)
                self.assertIn("a rehearsal `up` never restores", result.stderr)
                self.assertIn(f"The source volume {t.PROJECT}-postgres-data is absent and the repository under the "
                              "rehearsal prefix rehearsal/test already holds a backup", result.stderr)
                self.assertIn("`rehearsal.sh restore --target NEWDIR --json-out FILE [--target-time UTC]`", result.stderr)
                self.assertEqual(self.ups(), [], "Compose was started")
                # The only thing that ran besides the image check: one mount-free reader of the repository.
                self.assertEqual(self.mutating(), self.readers())
                self.assertEqual([call[-4:] for call in self.readers()], [["exec", PGBACKREST, "--output=json", "info"]])
                self.assertIn(f"--network={t.PROJECT}-network", self.readers()[0])
                self.assertFalse(any(arg in ("-v", "--volume", "--mount") for arg in self.readers()[0]))
                left = json.loads(self.state_file.read_text())
                self.assertEqual([e["Name"] for e in left["volumes"]], ["komika_server-data"])  # No volume was created.
                self.assertEqual(left["containers"], [])

    def test_an_empty_repository_or_an_existing_volume_starts_as_before(self):
        for name, info in (("no stanza", "[]"), ("a stanza without a backup", json.dumps([dict(INFO[0], backup=[])]))):
            with self.subTest(repository=name):
                self.state["run"] = {"info": info}
                self.ok("up")
                self.assertEqual(len(self.ups()), 1)
                self.assertEqual(len(self.readers()), 1)
        # The data volume exists: what it holds is the image's question (test_hosted), no reader runs.
        self.state["run"] = {"info": json.dumps(INFO)}
        self.state["volumes"] += self.created
        self.state["up_creates"] = {"containers": [t.container()]}
        self.ok("up")
        self.assertEqual((len(self.ups()), self.readers()), (1, []))

    def test_what_the_host_cannot_read_is_left_to_the_image_which_refuses_too(self):
        # No rehearsal network yet (the very first `up`): no reader can run before Compose.
        self.state["networks"] = [e for e in self.state["networks"] if not e["Name"].startswith(t.PROJECT)]
        result = self.ok("up")
        self.assertIn("refuses to restore an existing backup", result.stdout)
        self.assertEqual((len(self.ups()), self.readers()), (1, []))
        # The reader failed: it is said, and `up` goes on to the image's own refusal.
        self.state["networks"].append(network(t.PROJECT + "-network", "172.30.21.0/24"))
        self.state["volumes"] = [e for e in self.state["volumes"] if not e["Name"].startswith(t.PROJECT)]
        self.state["containers"] = []
        self.state["run_status"] = 1
        result = self.ok("up")
        self.assertIn("could not be read from the host", result.stdout)
        self.assertEqual(len(self.ups()), 1)
        # The image side of the same rule, in the file that the image really runs.
        source = (safety.KIT / "hosted.py").read_text()
        self.assertIn("if backups and safety.rehearsal_present():", source)
        self.assertLess(source.index("if backups and safety.rehearsal_present():"), source.index('pgbr("restore")'))

    def test_a_stack_without_a_repository_asks_nothing(self):
        self.no_repository()
        self.absent_project()
        self.state["networks"].append(network(t.PROJECT + "-network", "172.30.21.0/24"))
        self.state["up_creates"] = {"containers": [t.container(no_repo=True)], "volumes": self.created}
        self.ok("up")
        self.assertEqual((len(self.ups()), self.readers()), (1, []))


class OperationLockRunTests(JobFiles):
    """C4: one operation at a time on a host; a second invocation is refused at once."""

    def hold(self):
        """A real `rehearsal.sh import` whose job keeps running: it holds the lock."""
        import signal
        import time
        self.state["run_lingers"] = 60
        kit = self.prepare_process()
        process = subprocess.Popen(["sh", str(kit / "rehearsal.sh"), "import", *self.job], env=self.env,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, stdin=subprocess.DEVNULL,
                                   preexec_fn=lambda: signal.signal(signal.SIGTERM, signal.SIG_DFL))
        self.addCleanup(process.kill)
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            time.sleep(0.05)
            try:
                if any("-job-" in e["Name"] for e in json.loads(self.state_file.read_text())["containers"]):
                    break
            except ValueError:
                continue
        else:
            self.fail("the first import never started its job")
        self.live_state = True  # The first process owns the fake daemon's state from here on.
        return process

    def test_a_second_invocation_is_refused_at_once_before_it_checks_or_measures_anything(self):
        """The checker's two-admission case: the first step passed its disk check and
        is still writing; the second must not be admitted against the same free bytes."""
        import signal
        import time
        first = self.hold()
        # The plain fact first: a second step is not admitted and reaches nothing.
        result = self.run_rehearsal("up")
        self.assertEqual(result.returncode, 1, self.output)
        self.assertIn("holds the host-wide operation lock", result.stderr)
        self.assertEqual(self.calls, [], "the second invocation reached Docker")
        holder = safety.read_ledger(self.lock_path)[0]
        self.assertEqual(holder["subcommand"], "import")
        target = ["--target", str(self.root / "r"), "--json-out", str(self.root / "r.json")]
        commands = (["up"], ["restore", *target], ["drill", *target], ["backup", "full"], ["backup", "incr"],
                    ["import", *self.job], ["verify", *self.job], ["verify-restored", "--target", str(self.root), *self.job],
                    ["destroy-source-volumes", "--yes-delete-rehearsal-volumes"], ["repo-destroy", "--yes-delete-rehearsal-repo"],
                    ["stop-source"], ["down"], ["restore-cleanup", "--target", str(self.root)], ["preflight"],
                    ["preflight", "--init-root"], ["stanza-create"], ["check"], ["info"], ["info", "--json"], ["repo-usage"],
                    ["sql-marker", "--label", "a"], ["record-settings", "--out", str(self.root / "s.json")])
        self.assertEqual({command[0] for command in commands} | set(rehearsal.READ_ONLY), set(rehearsal.SPEC))
        with patch.object(safety, "free_bytes") as measured, patch.object(safety, "parse_env_file") as validated:
            for index, command in enumerate(commands):
                with self.subTest(command=" ".join(command[:2])):
                    before = time.monotonic()
                    result = self.run_rehearsal(*command, shell=index < 2)
                    self.assertLess(time.monotonic() - before, 5.0)  # Refused, never queued behind the first.
                    self.assertEqual(result.returncode, 1, self.output)
                    self.assertIn(f"another rehearsal.sh invocation holds the host-wide operation lock {self.lock_path} "
                                  f"(pid {holder['pid']}, subcommand import, since {holder['since']})", result.stderr)
                    self.assertIn("nothing was checked or started", result.stderr)
                    self.assertEqual(self.calls, [], "the second invocation reached Docker")
                    self.assertEqual(result.stdout, "")
            measured.assert_not_called()   # No free-space reading against bytes the first is still filling.
            validated.assert_not_called()  # The lock comes before any validation.
        self.assertFalse((self.root / "r").exists())
        self.assertEqual(safety.read_ledger(self.lock_path)[0], holder)  # No loser wrote into the record.
        # Read-only subcommands work while the operation runs, and say who holds the lock.
        result = self.ok("status")
        self.assertIn(f"operation recorded: subcommand=import pid={holder['pid']} since={holder['since']} process=running",
                      result.stdout)
        self.assertRegex(result.stdout, "job container " + re.escape(t.PROJECT) + "-job-[0-9a-f]{8}: state=running")
        for command in (["psql-check"], ["archive-status"], ["logs"]):
            self.ok(*command)
        self.assertIsNone(first.poll(), "the first import must still be running")
        # The first ends (here: by a signal); its cleanup ran under the lock; the next step is admitted.
        first.send_signal(signal.SIGTERM)
        out, err = first.communicate(timeout=60)
        self.assertEqual(first.returncode, 143, out + err)
        self.assertIn("was still present and has been removed", out)
        self.assertIsNone(safety.read_ledger(self.lock_path)[0])
        self.sync()
        self.live_state = False
        self.state["run_lingers"] = 0
        self.ok("backup", "incr")
        self.ok("up")

    def test_the_lock_precedes_validation_and_is_held_through_the_disk_gate_and_the_cleanup(self):
        seen = []

        def probe(what):
            try:
                safety.OperationLock().acquire("probe").release()
                seen.append((what, "free"))
            except ValueError:
                seen.append((what, "held"))

        def wrapped(function, what):
            def call(*args, **kwargs):
                probe(what)
                return function(*args, **kwargs)
            return call

        with patch.object(safety, "process_environment", wrapped(safety.process_environment, "validation")), \
                patch.object(safety.Rehearsal, "disk_gate", wrapped(safety.Rehearsal.disk_gate, "disk gate")), \
                patch.object(safety.Rehearsal, "remove_transient", wrapped(safety.Rehearsal.remove_transient, "cleanup")):
            self.ok("import", *self.job)
            self.assertEqual({what for what, _ in seen}, {"validation", "disk gate", "cleanup"})
            self.assertEqual({state for _, state in seen}, {"held"}, seen)
            probe("after the step")
            # A refused step and a failing step give the lock back too.
            self.refused("up", DOCKER_HOST="tcp://remote:2375")
            probe("after a refusal")
            self.state["run_status"] = 3
            self.assertEqual(self.run_rehearsal("verify", *self.job).returncode, 3)
            probe("after a failed job")
        self.assertEqual([entry for entry in seen if entry[0].startswith("after")],
                         [("after the step", "free"), ("after a refusal", "free"), ("after a failed job", "free")])
        self.assertEqual({state for what, state in seen if not what.startswith("after")}, {"held"}, seen)
        self.assertEqual(safety.read_ledger(self.lock_path), (None, []))
        # The read-only subcommands never take it.
        seen.clear()
        with patch.object(safety, "process_environment", wrapped(safety.process_environment, "validation")):
            for command in rehearsal.READ_ONLY:
                self.ok(command)
        self.assertEqual(seen, [("validation", "free")] * len(rehearsal.READ_ONLY))

    def test_usage_errors_and_read_only_subcommands_never_create_the_lock_file(self):
        for command in (["status"], ["psql-check"], ["archive-status"], ["logs"]):
            self.ok(*command)
        self.run_rehearsal("up", "--force")
        self.run_rehearsal("lock")
        self.assertFalse(os.path.lexists(self.lock_path))
        self.ok("up")
        self.assertEqual((stat_mode(self.lock_path), os.lstat(self.lock_path).st_uid), (0o600, os.getuid()))
        self.assertEqual(safety.read_ledger(self.lock_path), (None, []))
        self.assertIn(f"operation_lock={self.lock_path}", self.ok("preflight").stdout)
        self.assertIn("operation recorded: none", self.ok("status").stdout)

    def test_a_lock_path_that_is_not_the_callers_plain_file_stops_every_step_before_docker(self):
        production = self.base / "production.sqlite3"
        production.write_bytes(b"production bytes")

        def link():
            os.symlink(production, self.lock_path)

        def wide():
            Path(self.lock_path).write_text("{}")
            os.chmod(self.lock_path, 0o666)

        def second_name():
            Path(self.lock_path).write_text("{}")
            os.chmod(self.lock_path, 0o600)
            os.link(self.lock_path, self.base / "other-name")

        for name, make in (("a link to a production file", link), ("mode 0666", wide), ("two names", second_name)):
            make()
            for command in (["up"], ["import", *self.job], ["down"], ["preflight"]):
                for shell in (False, command == ["up"]):
                    with self.subTest(lock=name, command=command[0], shell=shell):
                        result = self.run_rehearsal(*command, shell=shell)
                        self.assertEqual(result.returncode, 1, self.output)
                        self.assertIn("must be a regular file of the caller", result.stderr)
                        self.assertEqual(self.calls, [])
            # `status` reads without locking: it says so and still reports.
            self.assertIn("operation lock: unreadable", self.ok("status").stdout)
            self.assertEqual(production.read_bytes(), b"production bytes")
            os.unlink(self.lock_path)
        # A rehearsal root may not contain the lock.
        result = self.refused("import", *self.job, KOMIKA_REHEARSAL_ROOT=str(self.base))
        self.assertIn("rehearsal root must not contain the operation lock", result.stderr)


class UnresolvedTests(JobFiles):
    """C6: an interrupted creation that was not seen is unresolved, never "absent"."""

    def interrupt(self, late, **constants):
        """Kill a real `import` while its create request is on its way. With
        late="gate" the fake daemon creates the container only when the test
        calls arrive(): no assertion depends on how fast this machine is."""
        import signal
        self.state["run_late"] = late
        if self.gate.exists():
            self.gate.unlink()
        kit = self.prepare_process(**constants)
        process = subprocess.Popen(["sh", str(kit / "rehearsal.sh"), "import", *self.job], env=self.env,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, stdin=subprocess.DEVNULL,
                                   preexec_fn=lambda: signal.signal(signal.SIGTERM, signal.SIG_DFL))
        self.addCleanup(process.kill)
        name = None
        deadline = time.monotonic() + 60
        while name is None and time.monotonic() < deadline:
            time.sleep(0.02)
            for line in (self.log.read_text().splitlines() if self.log.exists() else []):
                call = json.loads(line)["args"]
                if call[0] == "run" and "sha256sum" not in call:
                    name = call[call.index("--name") + 1]
        self.assertIsNotNone(name, "the job never reached docker run")
        process.send_signal(signal.SIGTERM)
        out, err = process.communicate(timeout=60)
        self.assertEqual(process.returncode, 143, out + err)
        self.live_state = True  # The late creation is still on its way into the state file.
        return name, out, err

    @property
    def gate(self):
        return self.base / "bin" / "late-gate"

    def present(self, name):
        try:
            return any(e["Name"] == "/" + name for e in json.loads(self.state_file.read_text())["containers"])
        except ValueError:  # The fake daemon is rewriting its state file this instant.
            return False

    def arrive(self, name):
        """The daemon now creates the container whose client is long dead."""
        self.assertFalse(self.present(name))
        self.gate.write_text("")
        deadline = time.monotonic() + 300
        while not self.present(name) and time.monotonic() < deadline:
            SLEEP(0.02)
        self.assertTrue(self.present(name), "the late container never appeared")

    def large(self):
        target = ["--target", str(self.root / "r"), "--json-out", str(self.root / "r.json")]
        return (["up"], ["backup", "full"], ["import", *self.job], ["verify", *self.job], ["restore", *target],
                ["drill", *target], ["stanza-create"], ["info"], ["repo-usage"],
                ["destroy-source-volumes", "--yes-delete-rehearsal-volumes"], ["repo-destroy", "--yes-delete-rehearsal-repo"])

    def refused_everywhere(self, name, how):
        for command in self.large():
            with self.subTest(command=command[0], how=how):
                result = self.refused(*command)
                self.assertRegex(result.stderr, "unresolved job container\\(s\\) of an interrupted earlier step: "
                                 + re.escape(name) + " \\(" + how + "\\)")
                self.assertIn("`rehearsal.sh stop-source` (or `down`)", result.stderr)
                self.assertFalse(any(identity_call(call) for call in self.calls))
        result = self.refused("preflight")
        self.assertIn(f"REFUSAL: unresolved jobs: unresolved job container(s) of an interrupted earlier step: {name}", result.stdout)

    def test_a_creation_that_arrives_after_the_settle_window_is_reported_found_and_removed(self):
        """The checker's state: the daemon creates the container later than the wait."""
        import hashlib
        name, out, err = self.interrupt("gate", SETTLE_SECONDS=0.5)
        # Not "absent": unresolved, by name, on both streams and in the lock file's record.
        self.assertIn(f"UNRESOLVED: the request that creates container {name} was interrupted", err)
        self.assertIn("It is NOT known to be absent", err)
        self.assertIn(f"job_container={name} UNRESOLVED", out)
        self.assertNotIn("has been removed", out)
        self.assertEqual(self.ledger(), [name])
        self.assertFalse(self.present(name))
        # Before it exists: `status` reports it and every large step refuses.
        result = self.ok("status")
        self.assertIn(f"UNRESOLVED job container {name}: its creation was interrupted", result.stdout)
        self.assertIn("`stop-source` or `down` reconciles it", result.stdout)
        result = self.refused("up")
        self.assertIn(f"interrupted earlier step: {name} (creation never confirmed)", result.stderr)
        # It does not exist and is not known to be absent: no large step starts.
        self.refused_everywhere(name, "creation never confirmed")
        self.assertFalse(self.present(name))
        # The daemon creates it after all: after the settle window, after the
        # process has ended and after every refusal above, however long they took.
        self.arrive(name)
        result = self.ok("status")
        self.assertIn(f"job container {name}: state=running", result.stdout)
        self.assertIn(f"UNRESOLVED job container {name}", result.stdout)
        self.refused_everywhere(name, "present")
        # `stop-source` removes it and clears the record; then steps run again.
        with patch.object(safety, "UNRESOLVED_AGE", 0.1):
            result = self.ok("stop-source")
        self.assertIn(["rm", "-f", "-v", hashlib.sha256(name.encode()).hexdigest()], self.calls)
        self.assertIn(f"unresolved job container {name}: it had appeared and was removed; record cleared", result.stdout)
        self.assertEqual(self.ledger(), [])
        self.assertFalse(self.present(name))
        self.assertNotIn("UNRESOLVED", self.ok("status").stdout)
        self.live_state = False
        self.state["run_late"] = 0
        self.state["containers"] = [t.container()]
        self.ok("backup", "incr")
        self.ok("import", *self.job)

    def test_stop_source_and_down_wait_for_a_creation_that_is_still_on_its_way(self):
        import hashlib
        for command in ("stop-source", "down"):
            with self.subTest(command=command):
                self.state["containers"] = [t.container()]
                self.live_state = False
                name, out, err = self.interrupt("gate", SETTLE_SECONDS=0.3)
                self.assertEqual(self.ledger(), [name])
                self.assertFalse(self.present(name))
                waits = []

                def sleep(seconds):
                    # The kit's own wait, before it has removed anything: the daemon
                    # creates the container while that wait lasts.
                    waits.append(seconds)
                    if len(waits) == 1:
                        done = [json.loads(line)["args"][0] for line in self.log.read_text().splitlines()]
                        self.assertFalse({"rm", "stop"} & set(done), done)
                        self.arrive(name)
                    else:
                        SLEEP(min(seconds, 0.05))

                with patch.object(safety, "UNRESOLVED_AGE", 600.0), patch.object(time, "sleep", sleep):
                    result = self.ok(command)  # Started before the container exists.
                self.assertTrue(waits and 0 < waits[0] <= 600.0, waits)
                self.assertIn(f"the creation of {name} was interrupted less than 600 s ago", result.stdout)
                self.assertIn(["rm", "-f", "-v", hashlib.sha256(name.encode()).hexdigest()], self.calls)
                self.assertIn("it had appeared and was removed; record cleared", result.stdout)
                self.assertEqual(self.ledger(), [])
                self.assertFalse(self.present(name))

    def test_a_creation_that_never_arrives_is_cleared_only_by_stop_source_or_down(self):
        def killed(self_, *args, input=None):
            if args[0] == "run":
                raise KeyboardInterrupt  # The client died before the daemon answered; nothing is ever created.
            return 0

        with patch.object(safety.Rehearsal, "stream", killed), patch.object(safety, "SETTLE_SECONDS", 0.3):
            result = self.run_rehearsal("import", *self.job)
        self.assertEqual(result.returncode, 130, self.output)
        self.assertEqual(len(self.ledger()), 1)
        name = self.ledger()[0]
        self.assertIn(f"UNRESOLVED: the request that creates container {name} was interrupted", result.stderr)
        self.refused_everywhere(name, "creation never confirmed")
        # A record of another project on the same host is reported, and blocks nobody here.
        lock = safety.OperationLock().acquire("import")
        lock.note("komika-pg-rehearsal-other-job-00000001")
        lock.resolve([name])
        lock.release()
        self.assertIn("UNRESOLVED job container komika-pg-rehearsal-other-job-00000001: its creation was interrupted",
                      self.ok("status").stdout)
        self.assertIn("it belongs to another rehearsal project on this host", self.ok("status").stdout)
        self.ok("backup", "incr")
        lock = safety.OperationLock().acquire("import")
        lock.note(name)
        lock.release()
        with patch.object(safety, "UNRESOLVED_AGE", 0.6):
            result = self.ok("down")
        self.assertRegex(result.stdout, f"unresolved job container {name}: it never appeared in [0-9]+ s; record cleared")
        self.assertEqual(self.ledger(), ["komika-pg-rehearsal-other-job-00000001"])  # Not ours: left alone.
        self.ok("import", *self.job)

    def test_a_forged_record_cannot_stretch_the_wait_or_crash_a_step(self):
        """Fix round 6 (F4): the record is the caller's own file, and still not trusted
        for more than a name and a wait of at most UNRESOLVED_AGE."""
        name = t.PROJECT + "-job-0123abcd"

        def forge(noted=None, pid="4321"):
            entries = "" if noted is None else '{"name": "%s", "noted": %s}' % (name, noted)
            Path(self.lock_path).write_text('{"holder": {"pid": %s, "subcommand": "import", "since": '
                                            '"2026-10-12T00:00:00Z"}, "unresolved": [%s]}' % (pid, entries))
            os.chmod(self.lock_path, 0o600)

        waits = []
        with patch.object(time, "sleep", waits.append):
            # A time in the future (forged, or the clock went back): the entry counts,
            # and the cleanup waits the documented 30 seconds, not what the record asks for.
            for command in ("stop-source", "down"):
                for noted in (repr(time.time() + 1e6), repr(time.time() + 31), "4102444800"):
                    with self.subTest(command=command, noted=noted):
                        forge(noted)
                        del waits[:]
                        self.assertIn(f"UNRESOLVED job container {name}: its creation was interrupted 0 s ago",
                                      self.ok("status").stdout)
                        self.assertIn(f"{name} (creation never confirmed)", self.refused("up").stderr)
                        result = self.ok(command)
                        self.assertEqual([wait for wait in waits if wait > 1], [30.0])
                        self.assertIn(f"waiting 30 s: the creation of {name} was interrupted less than 30 s ago", result.stdout)
                        self.assertIn(f"unresolved job container {name}: it never appeared in 0 s; record cleared", result.stdout)
                        self.assertEqual(self.ledger(), [])
            # Not a time of day at all: no record, no wait, no crash.
            for noted in ("1e400", "-1e400", "-1", "NaN", "Infinity", "1" + "0" * 400, "4102444801", '"soon"', "true"):
                with self.subTest(noted=noted[:12]):
                    forge(noted)
                    del waits[:]
                    self.assertEqual(self.ledger(), [])
                    self.assertNotIn("UNRESOLVED", self.ok("status").stdout)
                    result = self.ok("stop-source")
                    self.assertNotIn("waiting", result.stdout)
                    self.assertEqual([wait for wait in waits if wait > 1], [])
                    self.ok("backup", "incr")
            # A process ID that no kernel has: `status` reads the record without crashing.
            for pid in ("1" + "0" * 30, str(1 << 40), "-5", "1"):
                with self.subTest(pid=pid[:12]):
                    forge(pid=pid)
                    self.assertIn("operation recorded: none", self.ok("status").stdout)
                    self.ok("stop-source")
        self.assertNotIn("Traceback", self.output)

    def test_an_interrupted_reader_or_image_check_is_unresolved_too(self):
        self.state["containers"] = []
        with patch.object(drill, "command", side_effect=KeyboardInterrupt), patch.object(safety, "SETTLE_SECONDS", 0.2):
            result = self.run_rehearsal("repo-usage")  # Its reader's client is killed before the daemon answers.
        self.assertEqual(result.returncode, 130, self.output)
        self.assertEqual(len(self.ledger()), 1)
        self.assertRegex(self.ledger()[0], re.escape(t.PROJECT) + "-meta-[0-9a-f]{8}")
        self.assertIn(f"UNRESOLVED: the request that creates container {self.ledger()[0]}", result.stderr)
        self.assertIn("unresolved job container(s)", self.refused("info").stderr)
        with patch.object(safety, "UNRESOLVED_AGE", 0.1):
            self.ok("stop-source")
        self.assertEqual(self.ledger(), [])


class RefusalTextTests(JobFiles):
    """Fix round 6 (F3, F8): a refusal says what is wrong and what to do, never only the class of an exception."""

    BARE = r"(?m)(?:refused|REFUSAL: [a-z ]+): [A-Za-z]*(?:Error|Exception)\s*$"

    def test_a_missing_root_names_the_path_and_the_step_that_creates_it(self):
        self.linux()  # `restore` and `drill` need the Linux mount table.
        absent = str(self.base / "no-such-root")
        target = ["--target", absent + "/r", "--json-out", absent + "/r.json"]
        for command in (["import", *self.job], ["verify", *self.job], ["verify-restored", "--target", absent + "/r", *self.job],
                        ["restore", *target], ["drill", *target], ["restore-cleanup", "--target", absent + "/r"],
                        ["record-settings", "--out", absent + "/s.json"], ["info", "--json", "--out", absent + "/i.json"],
                        ["repo-usage", "--out", absent + "/u.json"]):
            with self.subTest(command=command[0]):
                result = self.refused(*command, KOMIKA_REHEARSAL_ROOT=absent)
                self.assertIn(f"rehearsal refused: the rehearsal root {absent} does not exist: run "
                              "`rehearsal.sh preflight --init-root` first", result.stderr)
                self.assertNotRegex(self.output, self.BARE)
                self.assertNotIn("FileNotFoundError", self.output)
        result = self.refused("preflight", KOMIKA_REHEARSAL_ROOT=absent)
        self.assertIn(f"REFUSAL: rehearsal root: the rehearsal root {absent} does not exist: run "
                      "`rehearsal.sh preflight --init-root` first", result.stdout)
        self.assertNotRegex(self.output, self.BARE)
        self.assertFalse(os.path.lexists(absent))  # Saying it creates nothing.

    def test_a_missing_env_file_names_the_path_and_the_variable(self):
        absent = str(self.base / "no-such.env")
        for command in (["up"], ["status"], ["stop-source"], ["down"], ["import", *self.job]):
            with self.subTest(command=command[0]):
                result = self.refused(*command, KOMIKA_COMPOSE_ENV_FILE=absent)
                self.assertIn(f"rehearsal refused: the env file {absent} does not exist: KOMIKA_COMPOSE_ENV_FILE must "
                              "name this rehearsal's private env file", result.stderr)
                self.assertNotRegex(self.output, self.BARE)
                self.assertEqual(self.calls, [])
        result = self.refused("preflight", KOMIKA_COMPOSE_ENV_FILE=absent)
        self.assertIn(f"REFUSAL: selection: the env file {absent} does not exist", result.stdout)
        self.assertNotRegex(self.output, self.BARE)

    def test_an_operating_system_error_says_what_the_system_said_and_which_path(self):
        import errno
        denied = PermissionError(errno.EACCES, os.strerror(errno.EACCES), "/srv/some/file")
        with patch.object(safety, "parse_env_file", side_effect=denied):
            result = self.refused("up")
            self.assertIn("rehearsal refused: PermissionError: Permission denied (/srv/some/file). Check that the named "
                          "path exists and that the calling user may use it", result.stderr)
            result = self.refused("preflight")
            self.assertIn("REFUSAL: selection: PermissionError: Permission denied (/srv/some/file). Check that",
                          result.stdout)
        # An error without a path or a text still says something.
        with patch.object(safety, "parse_env_file", side_effect=OSError()):
            self.assertIn("rehearsal refused: OSError: the operating system refused the operation. Check that",
                          self.refused("up").stderr)
        # Anything else could carry inspect data: its class and what to do next, never its content.
        with patch.object(safety, "parse_env_file", side_effect=KeyError("inspect-data-must-not-print")):
            for command, stream in (("up", "stderr"), ("preflight", "stdout")):
                result = self.refused(command)
                self.assertIn("an unexpected internal error (KeyError), most likely an answer of Docker that does not "
                              "have the expected shape. The step stopped here; run `rehearsal.sh status` before "
                              "anything else", getattr(result, stream))
                self.assertNotIn("inspect-data-must-not-print", self.output)
                self.assertNotRegex(self.output, self.BARE)

    @unittest.skipUnless(os.getuid(), "root can write anywhere")
    def test_a_lock_directory_the_caller_cannot_write_is_named_as_that(self):
        closed = self.base / "closed"
        closed.mkdir(mode=0o700)
        os.chmod(closed, 0o555)
        self.addCleanup(os.chmod, closed, 0o700)
        path = str(closed / "komika-pg-rehearsal.lock")
        with patch.object(safety, "LOCK_PATH", path):
            for command in (["up"], ["stop-source"], ["import", *self.job], ["preflight"]):
                with self.subTest(command=command[0]):
                    result = self.refused(*command)
                    self.assertIn(f"rehearsal refused: the caller (uid {os.getuid()}) may not create the operation lock "
                                  f"{path}: its directory {closed} (owner uid {os.getuid()}, mode 0555) is not writable "
                                  "for the caller. The kit keeps its one lock file in a directory that is owned by root "
                                  "and sticky (drwxrwxrwt, as /run/lock is on Debian and Ubuntu)", result.stderr)
                    self.assertNotIn("it is something else", result.stderr)
                    self.assertEqual(self.calls, [])
            self.assertIn("operation recorded: none", self.ok("status").stdout)  # Read-only steps need no lock.
        self.assertEqual(os.listdir(closed), [])


class BudgetTests(Harness):
    """C7: the numbers of the storage and request budget stay on record."""

    def test_info_json_is_kept_in_a_new_file_through_the_allowlisted_command_only(self):
        self.state["exec"] = {"--output=json info": json.dumps(INFO)}
        out = self.root / "full-info.json"
        result = self.ok("info", "--json", "--out", str(out))
        self.assertEqual(self.mutating(), [["exec", "--user", "postgres", "a" * 64, PGBACKREST, "--output=json", "info"]])
        self.assertEqual(json.loads(out.read_text()), INFO)
        self.assertEqual(stat_mode(out), 0o600)
        self.assertIn(f"info_json={out}", result.stdout)
        self.assertEqual(json.loads(result.stdout.splitlines()[0]), INFO)
        # A new name directly under the root; never an overwrite, never without --json.
        for bad in (str(out), str(self.base / "elsewhere.json"), str(self.root / "a" / "b.json"), "relative.json",
                    str(self.root / safety.ROOT_MARKER)):
            with self.subTest(out=bad):
                self.refused("info", "--json", "--out", bad)
        self.assertEqual(json.loads(out.read_text()), INFO)
        for usage in (["info", "--out", str(self.root / "x.json")], ["info", "--json", "--out"],
                      ["info", "--json", "--out", str(self.root / "x.json"), "--out", str(self.root / "y.json")],
                      ["info", "--json", "--out", str(self.root / "x.json"), "--repo=2"]):
            with self.subTest(usage=usage[1:]):
                self.refused(*usage, status=2)
                self.assertEqual(self.calls, [])
        # What is no JSON list is not stored.
        for text in ("ERROR: [055]", '{"backup": []}'):
            self.state["exec"] = {"--output=json info": text}
            result = self.run_rehearsal("info", "--json", "--out", str(self.root / "bad.json"))
            self.assertEqual(result.returncode, 1, self.output)
            self.assertFalse((self.root / "bad.json").exists())
        # After the source is gone the same record comes from a mount-free reader.
        self.state["containers"] = []
        self.state["run"] = {"info": json.dumps(INFO)}
        self.ok("info", "--json", "--out", str(self.root / "incr-info.json"))
        self.assertEqual(json.loads((self.root / "incr-info.json").read_text()), INFO)
        readers = [call for call in self.mutating() if call[0] == "run"]
        self.assertEqual([call[-4:] for call in readers], [["exec", PGBACKREST, "--output=json", "info"]])
        self.assertEqual(self.mutating(), readers)

    def test_repo_usage_counts_the_rehearsal_prefix_and_can_name_no_other(self):
        self.state["exec"] = {"repo-ls": json.dumps(LISTING)}
        result = self.ok("repo-usage")
        self.assertEqual(self.mutating(), [["exec", "--user", "postgres", "a" * 64, PGBACKREST, "--recurse", "--output=json", "repo-ls"]])
        line = result.stdout.splitlines()[-1]
        self.assertRegex(line, r"^prefix=rehearsal/test measured_at=20[0-9]{2}-[0-9]{2}-[0-9]{2}T[0-9:]{8}Z objects=4 ")
        self.assertEqual(line.split(" ", 2)[2], " ".join(f"{key}={value}" for key, value in USAGE.items()))
        out = self.root / "usage-after-full.json"
        result = self.ok("repo-usage", "--out", str(out))
        record = json.loads(out.read_text())
        self.assertEqual({key: value for key, value in record.items() if key not in ("measured_at",)},
                         dict(USAGE, prefix="rehearsal/test"))
        self.assertEqual(stat_mode(out), 0o600)
        self.assertIn(f"repo_usage_json={out}", result.stdout)
        self.refused("repo-usage", "--out", str(out))  # Never an overwrite.
        # The source is gone: a mount-free reader, and the prefix that reaches it is the env file's.
        self.state["containers"] = []
        self.state["run"] = {"repo-ls": json.dumps(LISTING)}
        self.ok("repo-usage")
        readers = [record for record in self.records if record["args"][0] == "run" and not identity_call(record["args"])]
        self.assertEqual([record["args"][-5:] for record in readers],
                         [["exec", PGBACKREST, "--recurse", "--output=json", "repo-ls"]])
        self.assertEqual(readers[0]["stdin_values"]["KOMIKA_R2_PREFIX"], "rehearsal/test")
        self.assertEqual(readers[0]["stdin_values"]["KOMIKA_PRODUCTION_R2_PREFIX"], "komika")
        self.assertFalse(any(arg in ("-v", "--volume", "--mount") or arg.startswith("--repo") for arg in readers[0]["args"]))
        # It is read-only: nothing that deletes or writes is in any command line of this subcommand.
        self.assertFalse(any(word in json.dumps(self.calls) for word in ("repo-rm", "stanza-delete", "expire", "repo-put")))
        # No argument can select a prefix, a path or a repository.
        for usage in (["repo-usage", "komika"], ["repo-usage", "/"], ["repo-usage", "--prefix", "komika"],
                      ["repo-usage", "--repo1-path=/komika"], ["repo-usage", "--recurse"], ["repo-usage", "--out"],
                      ["repo-usage", "--stanza=komika"], ["repo-usage", "rehearsal/test"], ["repo-ls"], ["repo-ls", "/"]):
            with self.subTest(usage=usage):
                self.refused(*usage, status=2)
                self.assertEqual(self.calls, [], "a usage error must not call Docker at all")
        # An env file whose prefix is production's, or no rehearsal prefix, never reaches Docker.
        for prefix, production in (("komika", "komika"), ("komika", "live"), ("rehearsal/test", "rehearsal/test"),
                                   ("rehearsal", "komika"), ("rehearsal/../komika", "komika"), ("live/rehearsal/x", "komika")):
            with self.subTest(prefix=prefix, production=production):
                self.write_env(dict(t.VALUES, KOMIKA_R2_PREFIX=prefix, KOMIKA_PRODUCTION_R2_PREFIX=production))
                self.refused("repo-usage")
                self.assertEqual(self.calls, [])
        self.no_repository()
        self.assertIn("NO_REPO", self.refused("repo-usage").stderr)

    def test_the_restore_report_keeps_the_per_backup_sizes_and_the_prefix_totals(self):
        self.linux()
        self.state["containers"] = []
        self.state["volumes"] = self.state["volumes"][2:]
        self.state["run"] = {"info": json.dumps(INFO), "repo-ls": json.dumps(LISTING)}
        self.state["exec"] = {"pg_controldata": CONTROL, "SELECT NOT pg_is_in_recovery": "t", "SELECT current_user": "komika",
                              "FROM pg_database": "0", "FROM work": "work=1\nchapter=2\nusers=3", "du -sb": "12345\t/restore"}
        result = self.ok("restore", "--target", str(self.root / "r"), "--json-out", str(self.root / "r.json"))
        report = json.loads((self.root / "r.json").read_text())
        self.assertEqual(report["repository_usage"], USAGE)
        self.assertEqual([(b["label"], b["type"], b["database_size_bytes"], b["repository_delta_bytes"], b["timestamp_stop"])
                          for b in report["backups"]],
                         [("20261011-120000F", "full", 1000, 300, 1791720100),
                          ("20261011-120000F_20261011-130000I", "incr", 2000, 50, 1791723700)])
        self.assertEqual((report["wal_archive_min"], report["wal_archive_max"]),
                         ("000000010000000000000001", "000000010000000000000009"))
        self.assertIn(f"repository_bytes={USAGE['bytes']} repository_objects=4 wal_archive_bytes={USAGE['archive_bytes']}",
                      result.stdout)


def stat_mode(path):
    return os.stat(path).st_mode & 0o7777


if __name__ == "__main__":
    unittest.main()
