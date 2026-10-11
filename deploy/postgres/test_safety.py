"""Pure tests of the rehearsal contract: pinned file, exact topology, env file,
process environment, container contract and the host path/inode/mount fence."""
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import stat as stat_module
import subprocess
import tempfile
import time
import unittest
from unittest.mock import patch
import safety
import test_support as t

PRODUCTION_VOLUME = "/var/lib/docker/volumes/komika_server-data/_data"
SECRETS = (t.PASSWORD, t.SECRET, "synthetickey")


def bind(source, target="/x"):
    return {"type": "bind", "source": source, "target": target, "bind": {"create_host_path": True}}


def change(path, value):
    """Set model[path[0]]...[path[-1]] = value (or delete it when value is DELETE)."""
    def apply(model):
        node = model
        for key in path[:-1]:
            node = node[key]
        if value is DELETE:
            del node[path[-1]]
        else:
            node[path[-1]] = value
    return apply


def append(path, value):
    def apply(model):
        node = model
        for key in path:
            node = node[key]
        node.append(value)
    return apply


DELETE = object()
PG = ("services", "postgres")
BACKUP = ("services", "postgres-backup")
# Every way the third check found (or could find) to reach the host or production
# through the rendered configuration. Each must be refused on its own.
UNSAFE = {
    "extra bind mount of a production volume": append(PG + ("volumes",), bind(PRODUCTION_VOLUME)),
    "data volume replaced by a production bind": change(PG + ("volumes", 0), bind(PRODUCTION_VOLUME, "/var/lib/postgresql")),
    "docker socket bind": append(BACKUP + ("volumes",), bind("/var/run/docker.sock", "/var/run/docker.sock")),
    "foreign named volume": change(PG + ("volumes", 0, "source"), "server-data"),
    "volume target moved": change(PG + ("volumes", 1, "target"), "/var/lib/postgresql/18"),
    "volume subpath option": change(PG + ("volumes", 0, "volume"), {"subpath": "x"}),
    "privileged": change(PG + ("privileged",), True),
    "host network mode": change(PG + ("network_mode",), "host"),
    "published port": change(PG + ("ports",), [{"mode": "ingress", "target": 5432, "published": "5432", "protocol": "tcp"}]),
    "volumes_from": change(BACKUP + ("volumes_from",), ["komika-server-1"]),
    "added capability": change(PG + ("cap_add",), ["SYS_ADMIN"]),
    "device": change(PG + ("devices",), [{"source": "/dev/sda", "target": "/dev/sda", "permissions": "rwm"}]),
    "host pid namespace": change(PG + ("pid",), "host"),
    "host ipc namespace": change(PG + ("ipc",), "host"),
    "host user namespace": change(PG + ("userns_mode",), "host"),
    "security option": change(PG + ("security_opt",), ["seccomp=unconfined"]),
    "root user": change(PG + ("user",), "0"),
    "entrypoint override": change(PG + ("entrypoint",), ["/bin/sh"]),
    "command override": change(PG + ("command",), ["exec", "sh"]),
    "scheduler command changed": change(BACKUP + ("command",), ["exec", "pgbackrest"]),
    "build section": change(PG + ("build",), {"context": "/tmp", "dockerfile": "Dockerfile"}),
    "env_file": change(PG + ("env_file",), [{"path": "/root/.env", "required": True}]),
    "extra hosts": change(PG + ("extra_hosts",), ["postgres=10.0.0.1"]),
    "changed image": change(BACKUP + ("image",), "postgres:18"),
    "container named like production": change(PG + ("container_name",), "komika-postgres-1"),
    "label removed": change(PG + ("labels",), {}),
    "label of another project": change(BACKUP + ("labels", t.LABEL), "komika"),
    "prefix variable missing in postgres-backup": change(BACKUP + ("environment", "KOMIKA_R2_PREFIX"), DELETE),
    "production prefix in postgres-backup": change(BACKUP + ("environment", "KOMIKA_R2_PREFIX"), "komika"),
    "comparator missing in postgres-backup": change(BACKUP + ("environment", "KOMIKA_PRODUCTION_R2_PREFIX"), DELETE),
    "pgBackRest override variable": change(PG + ("environment", "PGBACKREST_REPO1_PATH"), "/komika"),
    "project variable changed": change(PG + ("environment", "KOMIKA_REHEARSAL_PROJECT"), "komika"),
    "allowed CIDR widened": change(PG + ("environment", "KOMIKA_PG_ALLOWED_CIDRS"), "172.21.0.0/16"),
    "extra network on a service": change(PG + ("networks",), {"rehearsal": None, "komika": None}),
    "network alias option": change(PG + ("networks",), {"rehearsal": {"aliases": ["postgres"]}}),
    "second top-level network": change(("networks", "komika"), {"name": "komika_komika", "external": True}),
    "external network": change(("networks", "rehearsal", "external"), True),
    "production network name": change(("networks", "rehearsal", "name"), "komika_komika"),
    "changed subnet": change(("networks", "rehearsal", "ipam", "config", 0, "subnet"), "172.21.0.0/16"),
    "host network driver": change(("networks", "rehearsal", "driver"), "host"),
    "network driver options": change(("networks", "rehearsal", "driver_opts"), {"com.docker.network.bridge.name": "br0"}),
    "volume driver_opts": change(("volumes", "postgres-data", "driver_opts"),
                                 {"type": "none", "o": "bind", "device": PRODUCTION_VOLUME}),
    "external volume": change(("volumes", "postgres-data", "external"), True),
    "production volume name": change(("volumes", "postgres-data", "name"), "komika_server-data"),
    "volume driver": change(("volumes", "postgres-socket", "driver"), "nfs"),
    "volume label removed": change(("volumes", "postgres-socket", "labels"), {}),
    "third service": change(("services", "server"), {"image": "komiq-server"}),
    "missing service": change(("services", "postgres-backup"), DELETE),
    "raised memory ceiling": change(PG + ("mem_limit",), "17179869184"),
    "missing memory limit": change(PG + ("mem_limit",), DELETE),
    "no pid limit": change(BACKUP + ("pids_limit",), DELETE),
    "restart policy": change(PG + ("restart",), "always"),
    "tmpfs widened": change(PG + ("tmpfs",), ["/run/komika:mode=0700", "/var/lib/docker"]),
    "secrets section": change(("secrets",), {"key": {"file": "/root/.env"}}),
    "configs section": change(("configs",), {"key": {"file": "/root/.env"}}),
    "wrong project name": change(("name",), "komika"),
    # Fix round 4 (F11): the health check command and the log driver are part of the contract.
    "postgres healthcheck command": change(PG + ("healthcheck", "test"), ["CMD", "true"]),
    "postgres healthcheck through a shell": change(PG + ("healthcheck", "test"), ["CMD-SHELL", "/opt/komika-postgres/health.sh"]),
    "postgres healthcheck disabled": change(PG + ("healthcheck",), {"disable": True}),
    "postgres healthcheck removed": change(PG + ("healthcheck",), DELETE),
    "backup healthcheck command": change(BACKUP + ("healthcheck", "test"), ["CMD", "python3", "-c", "pass"]),
    "healthcheck interval": change(PG + ("healthcheck", "interval"), "1s"),
    "healthcheck retries": change(BACKUP + ("healthcheck", "retries"), 300),
    "log driver none": change(PG + ("logging", "driver"), "none"),
    "log driver syslog": change(BACKUP + ("logging", "driver"), "syslog"),
    "unbounded log file": change(PG + ("logging", "options"), {}),
    "log size raised": change(BACKUP + ("logging", "options", "max-size"), "10g"),
    "logging removed": change(PG + ("logging",), DELETE),
    "TLS verification switched off in the rendering": change(PG + ("environment", "KOMIKA_R2_VERIFY_TLS"), "0"),
    "TLS variable missing in postgres-backup": change(BACKUP + ("environment", "KOMIKA_R2_VERIFY_TLS"), DELETE),
    "raised CPU ceiling": change(PG + ("cpus",), 2),
    "block I/O setting": change(PG + ("blkio_config",), {"weight": 10}),
}


class TopologyTests(unittest.TestCase):
    def test_pin_matches_the_checked_in_compose_file(self):
        actual = hashlib.sha256(safety.COMPOSE_FILE.read_bytes()).hexdigest()
        self.assertEqual(actual, safety.COMPOSE_SHA256,
                         "rehearsal-compose.yml changed: review the contract in safety.py, then update COMPOSE_SHA256")
        self.assertEqual(safety.compose_pin(), actual)

    def test_pin_refuses_any_changed_file(self):
        with tempfile.TemporaryDirectory() as directory:
            copy = Path(directory) / "rehearsal-compose.yml"
            copy.write_bytes(safety.COMPOSE_FILE.read_bytes() + b"# harmless looking comment\n")
            with self.assertRaisesRegex(ValueError, "pinned"):
                safety.compose_pin(copy)

    def test_literal_renderings_equal_the_contract(self):
        safety.check_topology(t.rendered(), t.PROJECT, t.VALUES)
        safety.check_topology(t.rendered(no_repo=True), t.PROJECT, t.NO_REPO_VALUES)
        # Other Compose releases print the same facts differently.
        model = t.rendered()
        for service in model["services"].values():
            service["networks"] = ["rehearsal"]
            service["stop_grace_period"] = "240s"
            service["mem_limit"] = int(service["mem_limit"])
            service["tmpfs"] = "/run/komika:mode=0700"
            del service["entrypoint"]
            for volume in service["volumes"]:
                del volume["volume"]
        del model["services"]["postgres"]["command"]
        model["services"]["postgres-backup"]["depends_on"]["postgres"] = {"condition": "service_healthy"}
        safety.check_topology(model, t.PROJECT, t.VALUES)

    def test_lowered_limits_are_part_of_the_contract_and_cannot_be_raised(self):
        values = dict(t.VALUES, KOMIKA_REHEARSAL_PG_MEMORY="2g", KOMIKA_REHEARSAL_BACKUP_PIDS="64")
        model = t.rendered()
        with self.assertRaises(ValueError):
            safety.check_topology(model, t.PROJECT, values)  # File says 2g, rendering says 4g.
        model["services"]["postgres"]["mem_limit"] = model["services"]["postgres"]["memswap_limit"] = str(2 << 30)
        model["services"]["postgres-backup"]["pids_limit"] = 64
        safety.check_topology(model, t.PROJECT, values)
        for key, value in (("KOMIKA_REHEARSAL_PG_MEMORY", "8g"), ("KOMIKA_REHEARSAL_PG_CPUS", "4"),
                           ("KOMIKA_REHEARSAL_BACKUP_PIDS", "4096"), ("KOMIKA_REHEARSAL_PG_SHM", "2g")):
            with self.subTest(key=key), self.assertRaises(ValueError):
                safety.expected_model(t.PROJECT, dict(t.VALUES, **{key: value}))

    def test_defaults_are_the_low_ones_and_the_ceilings_did_not_move(self):
        # F7: a rehearsal beside production starts small unless the env file asks for more.
        model = safety.expected_model(t.PROJECT, t.VALUES)
        self.assertEqual(model["services"]["postgres"]["mem_limit"], 3 << 30)
        self.assertEqual(model["services"]["postgres"]["memswap_limit"], 3 << 30)
        self.assertEqual(model["services"]["postgres"]["cpus"], 1.0)
        self.assertEqual(model["services"]["postgres-backup"]["cpus"], 1.0)
        self.assertEqual({name: default for name, (default, _) in safety.LIMITS.items()}, {
            "PG_MEMORY": "3g", "PG_SHM": "1g", "PG_CPUS": "1", "PG_PIDS": "256",
            "BACKUP_MEMORY": "512m", "BACKUP_CPUS": "1", "BACKUP_PIDS": "128",
            "JOB_MEMORY": "2g", "JOB_CPUS": "1", "JOB_PIDS": "256"})
        self.assertEqual({name: default for name, (default, _) in safety.RESTORE_LIMITS.items()},
                         {"MEMORY": "3g", "CPUS": "1", "PIDS": "128"})
        for name, ceiling in (("PG_MEMORY", 4 << 30), ("PG_CPUS", 2.0), ("JOB_MEMORY", 4 << 30), ("JOB_CPUS", 2.0)):
            self.assertEqual(safety.LIMITS[name][1], ceiling)
        self.assertEqual(safety.limit({"KOMIKA_REHEARSAL_PG_MEMORY": "4g"}, "KOMIKA_REHEARSAL_", "PG_MEMORY")[1], 4 << 30)

    def test_free_space_floor_is_an_integer_from_2_to_64(self):
        self.assertEqual(safety.floor_bytes({}), 3 << 30)
        self.assertEqual(safety.floor_bytes({"KOMIKA_REHEARSAL_MIN_FREE_GB": ""}), 3 << 30)
        self.assertEqual(safety.floor_bytes({"KOMIKA_REHEARSAL_MIN_FREE_GB": "2"}), 2 << 30)
        self.assertEqual(safety.floor_bytes({"KOMIKA_REHEARSAL_MIN_FREE_GB": "64"}), 64 << 30)
        for value in ("1", "65", "0", "3.5", "three", "-4", " 3", "003"):
            with self.subTest(value=value), self.assertRaisesRegex(ValueError, "from 2 to 64"):
                safety.floor_bytes({"KOMIKA_REHEARSAL_MIN_FREE_GB": value})
            with self.subTest(model=value), self.assertRaises(ValueError):
                safety.expected_model(t.PROJECT, dict(t.VALUES, KOMIKA_REHEARSAL_MIN_FREE_GB=value))

    def test_image_file_list_equals_what_the_dockerfile_copies(self):
        # F5: the identity check must cover every kit file that reaches the image.
        import re
        dockerfile = (safety.KIT / "Dockerfile").read_text()
        copies = re.findall(r"^COPY (.+) (\S+)$", dockerfile, re.MULTILINE)
        self.assertEqual(len(copies), 1, "a second COPY needs a second entry in safety.IMAGE_FILES")
        self.assertEqual(copies[0][1], safety.IMAGE_DIRECTORY + "/")
        self.assertEqual(sorted(copies[0][0].split()), sorted(safety.IMAGE_FILES))
        self.assertNotIn("ADD ", dockerfile)
        admitted = [line[1:] for line in (safety.KIT / ".dockerignore").read_text().splitlines() if line.startswith("!")]
        self.assertEqual(sorted(admitted), sorted(safety.IMAGE_FILES + ("Dockerfile", ".dockerignore")))
        for name in safety.IMAGE_FILES:
            self.assertTrue((safety.KIT / name).is_file())

    def test_local_store_rule_needs_rehearsal_local_project_and_private_literal_address(self):
        local = {"KOMIKA_REHEARSAL_PROJECT": t.PROJECT + "-local", "KOMIKA_R2_ENDPOINT": "10.213.4.1",
                 "KOMIKA_R2_VERIFY_TLS": "0"}
        self.assertTrue(safety.local_s3(local))
        for endpoint in ("172.16.0.1", "172.31.255.255", "192.168.0.1", "https://10.0.0.1"):
            self.assertTrue(safety.local_s3(dict(local, KOMIKA_R2_ENDPOINT=endpoint)))
        # Absent, empty or 1: verification stays on, whatever else is set.
        for value in (None, "", "1"):
            environment = {key: v for key, v in dict(local, KOMIKA_R2_VERIFY_TLS=value).items() if v is not None}
            self.assertFalse(safety.local_s3(environment))
            self.assertFalse(safety.local_s3({k: v for k, v in environment.items() if k != "KOMIKA_REHEARSAL_PROJECT"}))
        refused = [dict(local, KOMIKA_REHEARSAL_PROJECT=t.PROJECT), dict(local, KOMIKA_REHEARSAL_PROJECT="komika"),
                   dict(local, KOMIKA_REHEARSAL_PROJECT="komika-pg-rehearsal-local-1"),
                   {k: v for k, v in local.items() if k != "KOMIKA_REHEARSAL_PROJECT"},  # Hosted: not a rehearsal.
                   {k: v for k, v in local.items() if k != "KOMIKA_R2_ENDPOINT"}]
        refused += [dict(local, KOMIKA_R2_ENDPOINT=endpoint) for endpoint in (
            "example.r2.cloudflarestorage.com", "localhost", "8.8.8.8", "172.15.255.255", "172.32.0.0", "192.169.0.1",
            "11.0.0.1", "127.0.0.1", "169.254.1.1", "100.64.0.1", "0.0.0.0", "10.1.1", "10.1.1.1.1", "10.1.1.256",
            "010.1.1.1", "10.1.1.1:443", "http://10.1.1.1", "10.1.1.1/", "fd00::1", "[fd00::1]", "")]
        refused += [dict(local, KOMIKA_R2_VERIFY_TLS=value) for value in ("false", "no", "n", "00", " 0", "2")]
        for environment in refused:
            with self.subTest(environment=sorted(environment.items())), self.assertRaises(ValueError):
                safety.local_s3(environment)
        # The env file: same rule, and the container environment states the decision either way.
        values = dict(t.VALUES, KOMIKA_REHEARSAL_PROJECT=t.PROJECT + "-local", KOMIKA_R2_ENDPOINT="10.213.4.1")
        model = safety.expected_model(t.PROJECT + "-local", dict(values, KOMIKA_R2_VERIFY_TLS="0"))
        self.assertEqual(model["services"]["postgres-backup"]["environment"]["KOMIKA_R2_VERIFY_TLS"], "0")
        self.assertEqual(safety.expected_model(t.PROJECT + "-local", values)["services"]["postgres"]["environment"]["KOMIKA_R2_VERIFY_TLS"], "1")
        with self.assertRaisesRegex(ValueError, "-local"):
            safety.expected_model(t.PROJECT, dict(t.VALUES, KOMIKA_R2_ENDPOINT="10.213.4.1", KOMIKA_R2_VERIFY_TLS="0"))
        with self.assertRaisesRegex(ValueError, "private IPv4"):
            safety.expected_model(t.PROJECT + "-local", dict(values, KOMIKA_R2_ENDPOINT="example.r2.invalid", KOMIKA_R2_VERIFY_TLS="0"))

    def test_every_unsafe_effective_configuration_is_refused(self):
        for name, mutate in UNSAFE.items():
            with self.subTest(change=name):
                model = t.rendered()
                mutate(model)
                with self.assertRaises(ValueError) as refusal:
                    safety.check_topology(model, t.PROJECT, t.VALUES)
                message = str(refusal.exception)
                self.assertIn("rehearsal topology", message)
                for secret in SECRETS:
                    self.assertNotIn(secret, message)

    def test_unknown_service_setting_is_refused_without_a_deny_list(self):
        model = t.rendered()
        model["services"]["postgres"]["setting_invented_next_year"] = True
        with self.assertRaisesRegex(ValueError, "forbidden setting setting_invented_next_year"):
            safety.check_topology(model, t.PROJECT, t.VALUES)

    def test_prefix_fence_and_repository_values_of_the_env_file(self):
        for rehearsal, production in (("rehearsal/test", "rehearsal/test"), ("/rehearsal//test/", "rehearsal/test"),
                                      ("rehearsal/test", "/rehearsal//test/"), ("rehearsal/test/child", "rehearsal/test"),
                                      ("rehearsal/test", "rehearsal/test/child"), ("rehearsal/test", ""),
                                      ("production/child", "production"), ("komika", "production"),
                                      ("rehearsal/test", "rehearsal")):
            with self.subTest(rehearsal=rehearsal, production=production), self.assertRaises(ValueError):
                safety.expected_model(t.PROJECT, dict(t.VALUES, KOMIKA_R2_PREFIX=rehearsal,
                                                      KOMIKA_PRODUCTION_R2_PREFIX=production))
        safety.expected_model(t.PROJECT, dict(t.VALUES, KOMIKA_R2_PREFIX="/rehearsal//r1/",
                                              KOMIKA_PRODUCTION_R2_PREFIX="rehearsal/r10"))
        for missing in safety.R2_VARS + ("KOMIKA_PRODUCTION_R2_PREFIX", "KOMIKA_POSTGRES_PASSWORD"):
            with self.subTest(missing=missing), self.assertRaises(ValueError):
                safety.expected_model(t.PROJECT, {k: v for k, v in t.VALUES.items() if k != missing})
        with self.assertRaises(ValueError):
            safety.expected_model(t.PROJECT, dict(t.VALUES, KOMIKA_REHEARSAL_PROJECT="komika-pg-rehearsal-other"))
        for project in ("komika", "Komika-pg-rehearsal-x", "komika-pg-rehearsal-", "komika-pg-rehearsal-x-",
                        "komika-pg-rehearsal-x_y", "komika-pg-rehearsal-" + "x" * 25, "", None):
            with self.subTest(project=project), self.assertRaises(ValueError):
                safety.project_name(project)

    def test_no_repository_mode_refuses_every_repository_value(self):
        safety.expected_model(t.PROJECT, t.NO_REPO_VALUES)
        for key in safety.R2_VARS + ("KOMIKA_PRODUCTION_R2_PREFIX",):
            with self.subTest(key=key), self.assertRaisesRegex(ValueError, "NO_REPO"):
                safety.expected_model(t.PROJECT, dict(t.NO_REPO_VALUES, **{key: "anything"}))
        # A normal rendering is not accepted for a no-repository env file, and the reverse.
        with self.assertRaises(ValueError):
            safety.check_topology(t.rendered(), t.PROJECT, t.NO_REPO_VALUES)
        with self.assertRaises(ValueError):
            safety.check_topology(t.rendered(no_repo=True), t.PROJECT, t.VALUES)


class RealComposeTests(unittest.TestCase):
    """The same refusals through the real, daemon-free `docker compose config`."""

    @classmethod
    def setUpClass(cls):
        try:
            subprocess.run(["docker", "compose", "version"], capture_output=True, check=True, timeout=30)
        except (OSError, subprocess.SubprocessError):
            raise unittest.SkipTest("docker compose is not installed")

    def render(self, text, values=t.VALUES):
        import json
        with tempfile.TemporaryDirectory() as directory:
            compose = Path(directory) / "rehearsal-compose.yml"
            compose.write_text(text)
            env = Path(directory) / "synthetic.env"
            env.write_text("".join(f"{key}={value}\n" for key, value in values.items()))
            clean = {"PATH": os.environ["PATH"], "HOME": os.environ.get("HOME", "/")}
            result = subprocess.run(["docker", "compose", "--env-file", str(env), "-f", str(compose), "-p", t.PROJECT,
                                     "config", "--format", "json"], capture_output=True, text=True, env=clean)
            self.assertEqual(result.returncode, 0, result.stderr)
            return json.loads(result.stdout)

    def test_checked_in_file_renders_to_the_contract(self):
        text = safety.COMPOSE_FILE.read_text()
        safety.check_topology(self.render(text), t.PROJECT, t.VALUES)
        safety.check_topology(self.render(text, t.NO_REPO_VALUES), t.PROJECT, t.NO_REPO_VALUES)
        # The literal fixture and this Compose release state the same facts.
        self.assertEqual(safety.normalize(self.render(text)), safety.normalize(t.rendered()))

    def test_modified_files_render_to_refused_topologies(self):
        text = safety.COMPOSE_FILE.read_text()
        volumes = "    volumes: &volumes\n      - postgres-data:/var/lib/postgresql\n"
        edits = {
            "extra bind mount": (volumes, volumes + f"      - {PRODUCTION_VOLUME}:/x\n"),
            "production bind as data volume": ("      - postgres-data:/var/lib/postgresql\n",
                                               f"      - {PRODUCTION_VOLUME}:/var/lib/postgresql\n"),
            "docker socket": (volumes, volumes + "      - /var/run/docker.sock:/var/run/docker.sock\n"),
            "privileged": ("    init: true\n    restart: \"no\"\n    stop_grace_period: 240s\n    mem_limit: ${KOMIKA_REHEARSAL_PG_MEMORY:-3g}\n",
                           "    init: true\n    privileged: true\n    restart: \"no\"\n    stop_grace_period: 240s\n    mem_limit: ${KOMIKA_REHEARSAL_PG_MEMORY:-3g}\n"),
            "host network mode": ("    networks: [rehearsal]\n    healthcheck:\n      test: ['CMD', '/opt/komika-postgres/health.sh']\n",
                                  "    network_mode: host\n    healthcheck:\n      test: ['CMD', '/opt/komika-postgres/health.sh']\n"),
            "published port": ("    tmpfs: &tmpfs\n", "    ports: ['5432:5432']\n    tmpfs: &tmpfs\n"),
            "volumes_from": ("    command: [scheduler]\n", "    command: [scheduler]\n    volumes_from: [postgres]\n"),
            "volume driver_opts": ("    name: ${KOMIKA_REHEARSAL_PROJECT:?set rehearsal project}-postgres-data\n    driver: local\n",
                                   "    name: ${KOMIKA_REHEARSAL_PROJECT:?set rehearsal project}-postgres-data\n    driver: local\n"
                                   f"    driver_opts: {{type: none, o: bind, device: {PRODUCTION_VOLUME}}}\n"),
            "extra network": ("    networks: [rehearsal]\n    depends_on:\n", "    networks: [rehearsal, komika]\n    depends_on:\n",
                              "networks:\n  rehearsal:\n", "networks:\n  komika:\n    name: komika_komika\n    external: true\n  rehearsal:\n"),
            "changed image": ("    image: komika-postgres:18.6-pgbackrest-2.59\n    command: [scheduler]\n",
                              "    image: postgres:18\n    command: [scheduler]\n"),
            "prefix variable missing in postgres-backup": (
                "    environment: *environment\n",
                "    environment:\n      PGDATA: /var/lib/postgresql/18/docker\n      KOMIKA_REHEARSAL_PROJECT: ${KOMIKA_REHEARSAL_PROJECT}\n"),
            "entrypoint and user": ("    command: [scheduler]\n", "    command: [scheduler]\n    entrypoint: [/bin/sh]\n    user: '0'\n"),
            "added capability and host pid": ("    tmpfs: &tmpfs\n", "    cap_add: [SYS_ADMIN]\n    pid: host\n    tmpfs: &tmpfs\n"),
        }
        for name, pairs in edits.items():
            with self.subTest(edit=name):
                changed = text
                for old, new in zip(pairs[::2], pairs[1::2]):
                    self.assertEqual(text.count(old), 1, "test edit no longer matches the compose file")
                    changed = changed.replace(old, new)
                with tempfile.TemporaryDirectory() as directory:
                    copy = Path(directory) / "rehearsal-compose.yml"
                    copy.write_text(changed)
                    with self.assertRaisesRegex(ValueError, "pinned"):  # 1: the hash pin.
                        safety.compose_pin(copy)
                with self.assertRaisesRegex(ValueError, "rehearsal topology"):  # 2: the comparison alone.
                    safety.check_topology(self.render(changed), t.PROJECT, t.VALUES)


class EnvironmentTests(unittest.TestCase):
    def write(self, text, mode=0o600):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        path = Path(directory.name) / "rehearsal.env"
        path.write_text(text)
        path.chmod(mode)
        return path

    def test_env_file_accepts_only_private_strict_allowlisted_lines(self):
        text = "# comment\n\n" + "".join(f"{key}={value}\n" for key, value in t.VALUES.items())
        self.assertEqual(safety.parse_env_file(self.write(text), os.getuid()), t.VALUES)
        with self.assertRaisesRegex(ValueError, "0600"):
            safety.parse_env_file(self.write(text, 0o644), os.getuid())
        with self.assertRaisesRegex(ValueError, "0600"):
            safety.parse_env_file(self.write(text), os.getuid() + 1)
        link = self.write("") .with_name("link.env")
        link.symlink_to(self.write(text))
        with self.assertRaisesRegex(ValueError, "regular file"):
            safety.parse_env_file(link, os.getuid())
        secret = "leakedSECRETvalue"
        for line in ("COMPOSE_PROJECT_NAME=komika", "COMPOSE_FILE=/x.yml", "COMPOSE_PROFILES=all", "DOCKER_HOST=tcp://x:2375",
                     "COMPOSE_REMOVE_ORPHANS=1", "KOMIKA_REHEARSAL_IMAGE=postgres:18", "KOMIKA_PG_ALLOWED_CIDRS=10.0.0.0/8",
                     "PGBACKREST_REPO1_PATH=/komika", "export KOMIKA_R2_KEY=" + secret, "KOMIKA_R2_KEY='" + secret + "'",
                     'KOMIKA_R2_SECRET="' + secret + '"', "KOMIKA_R2_SECRET=" + secret + " # note",
                     "KOMIKA_R2_PREFIX=${KOMIKA_PRODUCTION_R2_PREFIX}", "KOMIKA_R2_PREFIX=rehearsal/../komika",
                     "KOMIKA_R2_KEY=" + secret + "\r", " KOMIKA_R2_KEY=" + secret, "KOMIKA_R2_KEY", "KOMIKA_POSTGRES_PASSWORD=short",
                     "KOMIKA_REHEARSAL_PG_MEMORY=4096", "KOMIKA_REHEARSAL_NO_REPO=yes",
                     "KOMIKA_R2_VERIFY_TLS=1", "KOMIKA_R2_VERIFY_TLS=false", "KOMIKA_R2_VERIFY_TLS=n",
                     "KOMIKA_REHEARSAL_MIN_FREE_GB=3.5", "KOMIKA_REHEARSAL_MIN_FREE_GB=100", "KOMIKA_REHEARSAL_EXTRA_NETWORK=bridge",
                     "KOMIKA_R2_STORAGE_HOST=10.0.0.1", "KOMIKA_REHEARSAL_PLATFORM=darwin", "KOMIKA_REHEARSAL_IO_WEIGHT=10",
                     "KOMIKA_R2_BUCKET=a\nKOMIKA_R2_BUCKET=b"):
            with self.subTest(line=line.split("=")[0]):
                with self.assertRaises(ValueError) as refusal:
                    safety.parse_env_file(self.write(line + "\n"), os.getuid())
                self.assertNotIn(secret, str(refusal.exception))

    def test_process_environment_allows_three_selection_variables_only(self):
        clean = {"PATH": "/usr/bin", "HOME": "/home/x", "KOMIKA_REHEARSAL_PROJECT": t.PROJECT,
                 "KOMIKA_REHEARSAL_ROOT": "/srv/r", "KOMIKA_COMPOSE_ENV_FILE": "/srv/e", "KOMIKA_TEST_POSTGRES_URL": "x"}
        safety.process_environment(clean)
        for key in ("DOCKER_HOST", "DOCKER_CONTEXT", "DOCKER_CONFIG", "DOCKER_TLS_VERIFY", "COMPOSE_FILE",
                    "COMPOSE_PROJECT_NAME", "COMPOSE_PROFILES", "COMPOSE_REMOVE_ORPHANS", "COMPOSE_ENV_FILES",
                    "KOMIKA_COMPOSE_FILE", "KOMIKA_COMPOSE_PROJECT", "KOMIKA_R2_PREFIX", "KOMIKA_R2_SECRET",
                    "KOMIKA_PRODUCTION_R2_PREFIX", "KOMIKA_POSTGRES_PASSWORD", "KOMIKA_REHEARSAL_NO_REPO",
                    "KOMIKA_REHEARSAL_PG_MEMORY", "KOMIKA_RESTORE_MEMORY", "KOMIKA_PG_INIT_NEW",
                    "KOMIKA_PG_ALLOWED_CIDRS", "PGBACKREST_REPO1_PATH", "KOMIKA_R2_VERIFY_TLS",
                    "KOMIKA_REHEARSAL_MIN_FREE_GB", "KOMIKA_REHEARSAL_PLATFORM"):
            with self.subTest(key=key), self.assertRaisesRegex(ValueError, key):
                safety.process_environment(dict(clean, **{key: ""}))

    def test_paths_must_be_resolved_and_free_of_mount_syntax(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            (root / "real").mkdir()
            (root / "link").symlink_to(root / "real")
            self.assertEqual(safety.canonical_path(str(root / "real"), "x"), str(root / "real"))
            for bad in (str(root / "link"), str(root / "real" / ".." / "real"), "relative", "", None,
                        str(root / "real") + "/", str(root) + "//real", str(root / "a,readonly=false"),
                        str(root / "a:b"), str(root / "a b"), str(root / "a\nb"), str(root / "a=b")):
                with self.subTest(path=bad), self.assertRaises(ValueError):
                    safety.canonical_path(bad, "x")
            self.assertEqual(safety.new_child(str(root / "new"), root, "x"), root / "new")
            for bad in (str(root / "real"), str(root / "link"), str(root / "real" / "nested"), str(root / ".hidden"),
                        str(root.parent / "sibling"), str(root / "a,b"), "new"):
                with self.subTest(child=bad), self.assertRaises(ValueError):
                    safety.new_child(bad, root, "x")


class ContainerContractTests(unittest.TestCase):
    def model(self, service="postgres", **values):
        return safety.expected_model(t.PROJECT, dict(t.VALUES, **values))["services"][service]

    def check(self, entry, service="postgres", **values):
        return safety.check_container(entry, t.PROJECT, service, self.model(service, **values), t.DOCKER_ROOT)

    def test_compose_created_container_matches_and_every_deviation_is_refused(self):
        self.check(t.container())
        self.check(t.container("postgres-backup"), "postgres-backup")
        # INIT_NEW may be returned to 0 in the env file without recreating the container.
        self.check(t.container(), KOMIKA_PG_INIT_NEW="0")
        production = {"Type": "bind", "Source": PRODUCTION_VOLUME, "Destination": "/x", "Mode": "", "RW": True}
        deviations = {
            "privileged": lambda e: e["HostConfig"].update(Privileged=True),
            "extra production bind": lambda e: e["Mounts"].append(production),
            "production bind instead of a volume": lambda e: e["Mounts"].__setitem__(0, dict(production, Destination="/var/lib/postgresql")),
            "foreign volume": lambda e: e["Mounts"][0].update(Name="komika_server-data"),
            "host network": lambda e: e["HostConfig"].update(NetworkMode="host"),
            "second network": lambda e: e["NetworkSettings"]["Networks"].update({"komika_komika": {}}),
            "published port": lambda e: e["HostConfig"].update(PortBindings={"5432/tcp": [{"HostPort": "5432"}]}),
            "bound port": lambda e: e["NetworkSettings"]["Ports"].update({"5432/tcp": [{"HostIp": "0.0.0.0", "HostPort": "5432"}]}),
            "capability": lambda e: e["HostConfig"].update(CapAdd=["SYS_ADMIN"]),
            "device": lambda e: e["HostConfig"].update(Devices=[{"PathOnHost": "/dev/sda"}]),
            "host pid": lambda e: e["HostConfig"].update(PidMode="host"),
            "host ipc": lambda e: e["HostConfig"].update(IpcMode="host"),
            "host userns": lambda e: e["HostConfig"].update(UsernsMode="host"),
            "volumes-from": lambda e: e["HostConfig"].update(VolumesFrom=["komika-server-1"]),
            "seccomp off": lambda e: e["HostConfig"].update(SecurityOpt=["seccomp=unconfined"]),
            "root user": lambda e: e["Config"].update(User="0"),
            "entrypoint": lambda e: e["Config"].update(Entrypoint=["/bin/sh"]),
            "command": lambda e: e["Config"].update(Cmd=["exec", "sh"]),
            "image": lambda e: e["Config"].update(Image="postgres:18"),
            "label": lambda e: e["Config"]["Labels"].pop(t.LABEL),
            "name": lambda e: e.update(Name="/komika-postgres-1"),
            "production prefix in the created container": lambda e: e["Config"].update(
                Env=[item if not item.startswith("KOMIKA_R2_PREFIX=") else "KOMIKA_R2_PREFIX=komika" for item in e["Config"]["Env"]]),
            "extra tmpfs": lambda e: e["HostConfig"]["Tmpfs"].update({"/var/lib/docker": ""}),
        }
        for name, deviate in deviations.items():
            with self.subTest(deviation=name):
                entry = t.container()
                deviate(entry)
                with self.assertRaises(ValueError) as refusal:
                    self.check(entry)
                for secret in SECRETS:
                    self.assertNotIn(secret, str(refusal.exception))

    def test_the_real_mount_source_decides_not_the_volume_name_and_destination(self):
        """C1: name, destination, driver and label all right, the bytes somewhere else."""
        for index in (0, 1):
            for source in (PRODUCTION_VOLUME, "/srv/komika/data", t.DOCKER_ROOT + "/volumes/komika_server-data/_data",
                           f"{t.DOCKER_ROOT}/volumes/{t.PROJECT}-postgres-data/_data/..", "", None):
                with self.subTest(mount=index, source=source):
                    entry = t.container()
                    entry["Mounts"][index]["Source"] = source
                    with self.assertRaisesRegex(ValueError, r"does not match the rehearsal contract \(mounts\)"):
                        self.check(entry)
        # The same container under another data root than the daemon's is not the contract either.
        with self.assertRaisesRegex(ValueError, r"\(mounts\)"):
            safety.check_container(t.container(), t.PROJECT, "postgres", self.model(), "/srv/other-docker")
        entry = t.container()
        entry["Mounts"][0]["Driver"] = "nfs"
        with self.assertRaisesRegex(ValueError, r"\(mounts\)"):
            self.check(entry)

    def test_live_resource_limits_must_equal_the_validated_model(self):
        """C2: each state the check accepted before: a limit removed, raised or of another kind."""
        host = lambda **change: (lambda e: e["HostConfig"].update(change))
        deviations = {
            "memory limit": [host(Memory=0), host(Memory=4 << 30), host(Memory=None), host(Memory="3221225472"),
                             lambda e: e["HostConfig"].pop("Memory")],
            "swap limit": [host(MemorySwap=0), host(MemorySwap=-1), host(MemorySwap=6 << 30),
                           lambda e: e["HostConfig"].pop("MemorySwap")],
            "CPU limit": [host(NanoCpus=0), host(NanoCpus=4000000000), host(NanoCpus=None),
                          host(NanoCpus=0, CpuQuota=400000, CpuPeriod=100000), host(NanoCpus=0, CpuQuota=-1),
                          host(CpuQuota=100000, CpuPeriod=100000), lambda e: e["HostConfig"].pop("NanoCpus")],
            "PID limit": [host(PidsLimit=0), host(PidsLimit=-1), host(PidsLimit=None), host(PidsLimit=4096),
                          lambda e: e["HostConfig"].pop("PidsLimit")],
            "shm size": [host(ShmSize=8 << 30), host(ShmSize=0), lambda e: e["HostConfig"].pop("ShmSize")],
            "logging": [host(LogConfig={"Type": "json-file", "Config": {}}), host(LogConfig=None),
                        host(LogConfig={"Type": "journald", "Config": {"max-file": "3", "max-size": "10m"}}),
                        host(LogConfig={"Type": "json-file", "Config": {"max-file": "3", "max-size": "10g"}}),
                        host(LogConfig={"Type": "json-file", "Config": {"max-file": "3", "max-size": "10m", "mode": "non-blocking"}})],
            "restart policy": [host(RestartPolicy={"Name": "always", "MaximumRetryCount": 0}),
                               host(RestartPolicy={"Name": "unless-stopped"})],
        }
        for service in safety.SERVICES:
            for reason, changes in deviations.items():
                for number, deviate in enumerate(changes):
                    with self.subTest(service=service, reason=reason, change=number):
                        entry = t.container(service)
                        deviate(entry)
                        with self.assertRaisesRegex(ValueError, r"does not match the rehearsal contract \(" + reason + r"\)"):
                            self.check(entry, service)
        # The same limit written as quota and period (what `docker update --cpus` leaves) is the same limit.
        entry = t.container()
        entry["HostConfig"].update(NanoCpus=0, CpuQuota=100000, CpuPeriod=100000)
        self.check(entry)
        # A limit that the env file changed after the container was created is no longer the contract.
        with self.assertRaisesRegex(ValueError, r"\(memory limit, swap limit\)"):
            self.check(t.container(), KOMIKA_REHEARSAL_PG_MEMORY="2g")
        with self.assertRaisesRegex(ValueError, r"\(CPU limit\)"):
            self.check(t.container(), KOMIKA_REHEARSAL_PG_CPUS="0.5")

    def test_environment_names_outside_the_model_are_refused_and_never_their_values(self):
        """C2: a variable that libpq, pgBackRest, the kit or the loader would obey."""
        extra = ["PGHOST=db.production.invalid", "PGPORT=5432", "PGPASSWORD=hunter2hunter2", "PGSSLMODE=disable",
                 "PGOPTIONS=-c fsync=off", "PGBACKREST_REPO1_PATH=/komika", "PGBACKREST_STANZA=production",
                 "PGBACKREST_REPO1_S3_KEY=productionkey", "KOMIKA_R2_EXTRA=x", "KOMIKA_HOSTED=1", "LD_PRELOAD=/x.so",
                 "PYTHONPATH=/x", "HOME=/x", "TZ=UTC", "PG_MAJOR=17", "PG_VERSION=17.2-1", "PATH=/x y",
                 "PGBACKREST_CONFIG=/x.conf", "LANG=C;x", "NOVALUE", "=x"]
        for item in extra:
            for service in safety.SERVICES:
                with self.subTest(item=item.split("=")[0], service=service):
                    entry = t.container(service)
                    if item.split("=")[0] in ("PG_MAJOR", "PG_VERSION", "PATH", "PGBACKREST_CONFIG", "LANG"):
                        entry["Config"]["Env"] = [e for e in entry["Config"]["Env"] if not e.startswith(item.split("=")[0] + "=")]
                    entry["Config"]["Env"].append(item)
                    with self.assertRaisesRegex(ValueError, "environment names outside the contract: ") as refusal:
                        self.check(entry, service)
                    value = item.partition("=")[2]
                    if len(value) > 3:
                        self.assertNotIn(value, str(refusal.exception))
        # A model name given twice: which value the process sees is not what was checked.
        entry = t.container()
        entry["Config"]["Env"].append("KOMIKA_R2_PREFIX=komika")
        with self.assertRaisesRegex(ValueError, "environment names outside the contract: KOMIKA_R2_PREFIX"):
            self.check(entry)
        # A model name that the container lacks, or carries with another value.
        for change in (lambda env: [e for e in env if not e.startswith("KOMIKA_R2_VERIFY_TLS=")],
                       lambda env: [e.replace("KOMIKA_R2_VERIFY_TLS=1", "KOMIKA_R2_VERIFY_TLS=0") for e in env],
                       lambda env: [e.replace("KOMIKA_PG_INIT_NEW=1", "KOMIKA_PG_INIT_NEW=2") for e in env]):
            entry = t.container()
            entry["Config"]["Env"] = change(entry["Config"]["Env"])
            with self.assertRaisesRegex(ValueError, r"\(environment \(run `rehearsal.sh up` to recreate\)\)"):
                self.check(entry)
        # The image's own names alone are no deviation; neither is their absence.
        entry = t.container()
        entry["Config"]["Env"] = [e for e in entry["Config"]["Env"] if not e.startswith(("GOSU_VERSION=", "LANG="))]
        self.check(entry)


class SourceStorageTests(unittest.TestCase):
    """C1: a label says who created a resource, never what it is backed by."""
    ROOT = "/var/lib/docker"
    TABLE = "25 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw\n"

    def volume(self, key="postgres-data", **change):
        name = f"{t.PROJECT}-{key}"
        return dict({"Name": name, "Driver": "local", "Options": None, "Scope": "local", "Labels": {t.LABEL: t.PROJECT},
                     "Mountpoint": f"{self.ROOT}/volumes/{name}/_data"}, **change)

    def problems(self, containers=None, volumes=None, table=None, platform="linux"):
        found = safety.source_storage_problems(
            t.PROJECT, self.ROOT, [t.container()] if containers is None else containers,
            [self.volume(), self.volume("postgres-socket")] if volumes is None else volumes,
            platform, self.TABLE if table is None else table)
        return [f"{kind} {name}: {reason}" for kind, name, reason in found]

    def foreign(self, mounts, name="komika-server-1", labels=None):
        return {"Id": "d" * 64, "Name": "/" + name, "Config": {"Labels": labels or {"com.docker.compose.project": "komika"}},
                "Mounts": mounts}

    def test_plain_local_volumes_and_their_two_containers_are_rehearsal_storage(self):
        production = self.foreign([{"Type": "volume", "Name": "komika_server-data", "Driver": "local",
                                    "Source": PRODUCTION_VOLUME, "Destination": "/data"},
                                   {"Type": "bind", "Source": "/srv/komika/Caddyfile", "Destination": "/etc/caddy/Caddyfile"}])
        other = {"Name": "komika_server-data", "Driver": "local", "Options": None, "Labels": {}, "Mountpoint": PRODUCTION_VOLUME}
        for platform in ("linux", "darwin"):
            self.assertEqual(self.problems([t.container(), t.container("postgres-backup"), production],
                                           [self.volume(), self.volume("postgres-socket"), other], platform=platform), [])
        self.assertEqual(self.problems([], []), [])  # Nothing exists yet: Compose creates plain volumes.
        self.assertEqual(self.problems([production], [other]), [])

    def test_a_labelled_volume_bound_to_a_production_directory_is_refused(self):
        """The checker's first accepted state: label and name are ours, the bytes are production's."""
        bound = {"type": "none", "o": "bind", "device": "/srv/komika/data"}
        cases = {
            "driver options": (self.volume(Options=bound), "has driver options"),
            "nfs options": (self.volume(Options={"type": "nfs", "o": "addr=10.0.0.1", "device": ":/export"}), "has driver options"),
            "another driver": (self.volume(Driver="rclone"), "driver is not `local`"),
            "no driver": (self.volume(Driver=None), "driver is not `local`"),
            "mountpoint elsewhere": (self.volume(Mountpoint="/srv/komika/data"), "mountpoint is not its own directory"),
            "mountpoint of another volume": (self.volume(Mountpoint=PRODUCTION_VOLUME), "mountpoint is not its own directory"),
            "mountpoint above": (self.volume(Mountpoint=f"{self.ROOT}/volumes/{t.PROJECT}-postgres-data"), "mountpoint is not"),
            "label lost": (self.volume(Labels={}), "lacks the selected rehearsal label"),
            "label of another project": (self.volume(Labels={t.LABEL: "komika-pg-rehearsal-other"}), "lacks the selected"),
        }
        for name, (volume, reason) in cases.items():
            for containers in ([], [t.container()]):
                with self.subTest(case=name, containers=len(containers)):
                    found = self.problems(containers, [volume, self.volume("postgres-socket")])
                    self.assertTrue(found and all(line.startswith(f"volume {t.PROJECT}-postgres-data: ") for line in found), found)
                    self.assertIn(reason, " ".join(found))
        # The socket volume is judged like the data volume.
        self.assertIn("has driver options", " ".join(self.problems([], [self.volume(), self.volume("postgres-socket", Options=bound)])))

    def test_something_mounted_on_the_volume_directory_is_refused_from_the_mount_table(self):
        path = f"{self.ROOT}/volumes/{t.PROJECT}-postgres-data/_data"
        for name, line in (
                ("a production directory bound onto it", f"41 25 8:1 /srv/komika/data {path} rw - ext4 /dev/sda1 rw\n"),
                ("another filesystem on it", f"41 25 8:33 / {path} rw - ext4 /dev/sdc1 rw\n"),
                ("a mount on the volume's own directory", f"41 25 8:33 / {self.ROOT}/volumes/{t.PROJECT}-postgres-data rw - ext4 /dev/sdc1 rw\n")):
            with self.subTest(case=name):
                found = self.problems(table=self.TABLE + line)
                self.assertEqual(len(found), 1, found)
                self.assertIn("is mounted at", found[0])
        # Docker's data root on its own filesystem is the normal case, not an alias.
        own = self.TABLE + f"30 25 8:17 / {self.ROOT} rw - ext4 /dev/sdb1 rw\n"
        self.assertEqual(self.problems(table=own), [])
        # Without a mount table (no Linux host) the inspected facts still decide.
        self.assertEqual(self.problems(platform="darwin", table=""), [])

    def test_a_service_container_whose_real_source_changed_is_refused(self):
        """The checker's second accepted state: volume name and destination kept, Source elsewhere."""
        for service in safety.SERVICES:
            for index in (0, 1):
                other = ("postgres-socket", "postgres-data")[index]  # The right directory of the OTHER volume.
                for source in ("/srv/komika/data", PRODUCTION_VOLUME, f"{self.ROOT}/volumes/{t.PROJECT}-{other}/_data"):
                    with self.subTest(service=service, mount=index, source=source):
                        entry = t.container(service)
                        entry["Mounts"][index]["Source"] = source
                        found = self.problems([entry])
                        self.assertIn(f"container {t.PROJECT}-{service}: its mounts are not exactly the two rehearsal "
                                      "volumes with their own source directories", " ".join(found))
        extra = t.container()
        extra["Mounts"].append({"Type": "bind", "Source": "/srv/komika/data", "Destination": "/x"})
        self.assertIn("its mounts are not exactly", " ".join(self.problems([extra])))
        fewer = t.container()
        fewer["Mounts"].pop()
        self.assertIn("its mounts are not exactly", " ".join(self.problems([fewer])))

    def test_no_other_container_may_mount_or_overlap_the_source_volumes(self):
        name = f"{t.PROJECT}-postgres-data"
        path = f"{self.ROOT}/volumes/{name}/_data"
        cases = {
            "by name": [{"Type": "volume", "Name": name, "Source": path, "Destination": "/data", "Driver": "local"}],
            "by path": [{"Type": "bind", "Source": path, "Destination": "/data"}],
            "a directory inside": [{"Type": "bind", "Source": path + "/18/docker", "Destination": "/data"}],
            "the directory above": [{"Type": "bind", "Source": f"{self.ROOT}/volumes/{name}", "Destination": "/data"}],
            "all volumes": [{"Type": "bind", "Source": f"{self.ROOT}/volumes", "Destination": "/data"}],
            "Docker's data root": [{"Type": "bind", "Source": self.ROOT, "Destination": "/data"}],
            "the whole host": [{"Type": "bind", "Source": "/", "Destination": "/host"}],
        }
        for case, mounts in cases.items():
            with self.subTest(case=case):
                found = self.problems([t.container(), self.foreign(mounts)])
                self.assertTrue(found and all(line.startswith(f"volume {t.PROJECT}-postgres-") for line in found), found)
                self.assertIn("komika-server-1", " ".join(found))
        # Our label on a container with another name does not make it one of the two services.
        impostor = self.foreign(cases["by name"], name="unexpected", labels={t.LABEL: t.PROJECT})
        self.assertIn("container unexpected, which is not a service of this rehearsal, mounts it",
                      " ".join(self.problems([t.container(), impostor])))
        # A service NAME without our label is a foreign container too.
        unlabelled = t.container()
        unlabelled["Config"]["Labels"] = {}
        self.assertIn("which is not a service of this rehearsal, mounts it", " ".join(self.problems([unlabelled])))
        # A foreign volume that claims the same directory.
        twin = {"Name": "komika_twin", "Driver": "local", "Options": None, "Labels": {}, "Mountpoint": path}
        self.assertIn("overlaps volume komika_twin", " ".join(self.problems(volumes=[self.volume(), self.volume("postgres-socket"), twin])))
        # The mount table: a foreign bind whose path is an alias of the volume's directory.
        alias = self.TABLE + f"41 25 8:1 {path} /srv/alias rw - ext4 /dev/sda1 rw\n"
        found = self.problems([t.container(), self.foreign([{"Type": "bind", "Source": "/srv/alias", "Destination": "/d"}])], table=alias)
        self.assertIn("is the same backing directory as container komika-server-1 mount (/srv/alias)", " ".join(found))

    def test_an_unusable_docker_root_cannot_be_judged(self):
        for root in ("/", "relative", "", None, "/var/lib/docker/", "/var/lib/../lib/docker", "/var/lib/docker data"):
            with self.subTest(root=root), self.assertRaisesRegex(ValueError, "data root"):
                safety.source_storage_problems(t.PROJECT, root, [], [], "linux", self.TABLE)


class OperationLockTests(unittest.TestCase):
    """C4: one rehearsal operation at a time on a host; the second is refused at once."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name).resolve()
        self.path = str(self.base / "komika-pg-rehearsal.lock")
        self.uid = os.getuid()

    def test_the_second_holder_is_refused_at_once_and_told_who_holds_it(self):
        saved = os.umask(0)
        try:
            first = safety.OperationLock(self.path).acquire("import")
        finally:
            os.umask(saved)
        info = os.lstat(self.path)
        self.assertEqual((stat_module.S_IMODE(info.st_mode), info.st_uid, info.st_nlink), (0o600, self.uid, 1))
        self.assertEqual(safety.read_ledger(self.path)[0]["subcommand"], "import")
        before = time.monotonic()
        with self.assertRaisesRegex(ValueError, "another rehearsal.sh invocation holds the host-wide operation lock "
                                    + re.escape(self.path) + rf" \(pid {os.getpid()}, subcommand import, since 20.*"
                                    "One operation at a time.*nothing was checked or started"):
            safety.OperationLock(self.path).acquire("restore")
        self.assertLess(time.monotonic() - before, 1.0)  # Refused, never queued.
        self.assertEqual(safety.read_ledger(self.path)[0]["subcommand"], "import")  # The loser wrote nothing.
        first.release()
        self.assertIsNone(safety.read_ledger(self.path)[0])
        second = safety.OperationLock(self.path).acquire("restore")
        self.assertEqual(safety.read_ledger(self.path)[0]["subcommand"], "restore")
        second.release()
        second.release()  # Idempotent.

    def test_the_kernel_frees_the_lock_when_its_holder_dies(self):
        reader, writer = os.pipe()
        child = os.fork()
        if child == 0:
            try:
                os.close(reader)
                safety.OperationLock(self.path).acquire("import")
                os.write(writer, b"1")
                time.sleep(60)
            finally:
                os._exit(0)
        os.close(writer)  # Only the child can answer; its death ends the read below.
        self.addCleanup(os.close, reader)
        self.reaped = False
        self.addCleanup(lambda: (os.kill(child, signal.SIGKILL) if not self.reaped else None))
        self.assertEqual(os.read(reader, 1), b"1")
        with self.assertRaisesRegex(ValueError, rf"pid {child}, subcommand import"):
            safety.OperationLock(self.path).acquire("up")
        os.kill(child, signal.SIGKILL)
        os.waitpid(child, 0)
        self.reaped = True
        lock = safety.OperationLock(self.path).acquire("up")  # No stale-lock handling is needed.
        self.assertEqual(safety.read_ledger(self.path)[0]["subcommand"], "up")
        lock.release()

    def test_only_the_callers_own_plain_0600_file_in_a_safe_directory_is_a_lock(self):
        target = self.base / "production.sqlite3"
        target.write_bytes(b"production bytes")
        cases = {}
        def link():
            os.symlink(target, self.path)
        def dangling():
            os.symlink(self.base / "absent", self.path)
        def hard():
            Path(self.path).write_text("")
            os.chmod(self.path, 0o600)
            os.link(self.path, self.base / "second-name")
        def wide():
            Path(self.path).write_text("")
            os.chmod(self.path, 0o644)
        def directory():
            os.mkdir(self.path)
        def fifo():
            os.mkfifo(self.path, 0o600)
        for name, make in (("a link to a production file", link), ("a dangling link", dangling), ("two names", hard),
                           ("mode 0644", wide), ("a directory", directory), ("a fifo", fifo)):
            with self.subTest(case=name):
                make()
                for attempt in (lambda: safety.OperationLock(self.path).acquire("up"), lambda: safety.read_ledger(self.path)):
                    with self.assertRaisesRegex(ValueError, "must be a regular file of the caller"):
                        attempt()
                self.assertEqual(target.read_bytes(), b"production bytes")
                self.assertFalse((self.base / "absent").exists())
                for leftover in (Path(self.path), self.base / "second-name"):
                    if leftover.is_dir() and not leftover.is_symlink():
                        leftover.rmdir()
                    elif os.path.lexists(leftover):
                        leftover.unlink()
        # Another user's file (the caller is given as another uid) is no lock of ours.
        Path(self.path).write_text("")
        os.chmod(self.path, 0o600)
        with patch.object(safety, "lock_directory"), \
                self.assertRaisesRegex(ValueError, "must be a regular file of the caller"):
            safety.OperationLock(self.path, uid=self.uid + 1).acquire("up")
        with self.assertRaisesRegex(ValueError, f"is owned by uid {self.uid}, not by root"):
            safety.OperationLock(self.path, uid=self.uid + 1).acquire("up")
        os.unlink(self.path)
        # The directory: a link, or writable by others without the sticky bit.
        real = self.base / "real"
        real.mkdir()
        (self.base / "alias").symlink_to(real)
        with self.assertRaisesRegex(ValueError, "must be a real directory"):
            safety.OperationLock(str(self.base / "alias" / "lock")).acquire("up")
        for mode, accepted in ((0o777, False), (0o775, False), (0o1777, True), (0o755, True), (0o700, True)):
            with self.subTest(mode=oct(mode)):
                os.chmod(real, mode)
                if accepted:
                    safety.OperationLock(str(real / "lock")).acquire("up").release()
                    os.unlink(real / "lock")
                else:
                    with self.assertRaisesRegex(ValueError, rf"\(mode {mode:04o}\) is writable by its group or by others "
                                                "and is not sticky"):
                        safety.OperationLock(str(real / "lock")).acquire("up")
                    self.assertFalse((real / "lock").exists())
        os.chmod(real, 0o700)
        with self.assertRaisesRegex(ValueError, "does not exist"):
            safety.OperationLock(str(self.base / "absent-directory" / "lock")).acquire("up")
        with self.assertRaisesRegex(ValueError, f"is owned by uid {self.uid}, not by root"):
            safety.lock_directory(str(real / "lock"), self.uid + 1)

    def test_a_lock_directory_that_is_missing_not_roots_or_not_sticky_is_named_as_exactly_that(self):
        """Fix round 6 (F8): each state of /run/lock that stops the kit gets its own sentence."""
        path = "/run/lock/komika-pg-rehearsal.lock"

        def state(mode, owner):
            return lambda directory: os.stat_result((mode, 1, 1, 2, owner, 0, 0, 0, 0, 0))

        def missing(directory):
            raise FileNotFoundError(2, "No such file or directory", directory)

        expected = "The kit keeps its one lock file in a directory that is owned by root and sticky"
        with patch.object(os.path, "realpath", lambda value: value):
            for lstat, text in (
                    (missing, "the operation lock directory /run/lock does not exist on this host"),
                    (state(stat_module.S_IFDIR | 0o1777, 4242), "the operation lock directory /run/lock is owned by uid 4242, not by root"),
                    (state(stat_module.S_IFDIR | 0o775, 0), r"the operation lock directory /run/lock \(mode 0775\) is writable by "
                                                           "its group or by others and is not sticky"),
                    (state(stat_module.S_IFDIR | 0o777, 0), r"/run/lock \(mode 0777\) is writable by its group or by others "
                                                           "and is not sticky"),
                    (state(stat_module.S_IFREG | 0o600, 0), "the operation lock directory /run/lock is not a directory")):
                with self.subTest(text=text):
                    with self.assertRaisesRegex(ValueError, text + ".*" + expected + ".*Nothing was checked or started"):
                        safety.lock_directory(path, 1000, lstat=lstat)
            # What a Debian or Ubuntu host has, and a private directory of the caller: accepted.
            for mode, owner in ((0o1777, 0), (0o755, 0), (0o700, 1000)):
                self.assertEqual(safety.lock_directory(path, 1000, lstat=state(stat_module.S_IFDIR | mode, owner)), "/run/lock")
        # Root's 0755 directory passes the directory rule, and then the caller cannot create
        # the file there: that is said as it is, not as "the lock is something else".
        closed = self.base / "closed"
        closed.mkdir(mode=0o700)
        os.chmod(closed, 0o555)
        self.addCleanup(os.chmod, closed, 0o700)
        if os.getuid():
            with self.assertRaisesRegex(ValueError, rf"the caller \(uid {self.uid}\) may not create the operation lock "
                                        + re.escape(str(closed / "lock")) + rf": its directory {re.escape(str(closed))} "
                                        rf"\(owner uid {self.uid}, mode 0555\) is not writable for the caller.*" + expected):
                safety.OperationLock(str(closed / "lock")).acquire("up")
            self.assertEqual(os.listdir(closed), [])
            self.assertEqual(safety.read_ledger(str(closed / "lock")), (None, []))  # Reading still works.

    def test_a_record_cannot_claim_an_impossible_time_or_process(self):
        """Fix round 6 (F4): the wait from the record is bounded; a forged time or pid is no record."""
        now = 1_800_000_000.0
        name = t.PROJECT + "-job-0123abcd"
        with patch.object(safety, "UNRESOLVED_AGE", 30.0):
            for noted, wait in ((now, 30.0), (now - 5, 25.0), (now - 30, 0.0), (now - 1e6, 0.0),
                                (now + 1, 30.0), (now + 1e6, 30.0), (safety.NOTED_MAX, 30.0), (0, 0.0)):
                with self.subTest(noted=noted):
                    result = safety.settle_wait([{"name": name, "noted": now - 1e5}, {"name": name, "noted": noted}], now)
                    self.assertEqual((result, type(result)), (wait, float))
            self.assertEqual(safety.settle_wait([], now), 0.0)
        # The newest of several decides, exactly as before.
        with patch.object(safety, "UNRESOLVED_AGE", 4.0):
            self.assertEqual(safety.settle_wait([{"name": name, "noted": now - 3}, {"name": name, "noted": now - 1}], now), 3.0)
        holder = '{"pid": %s, "subcommand": "import", "since": "2026-10-12T00:00:00Z"}'
        for noted in ("1e400", "-1e400", "-1", "-0.5", "NaN", "Infinity", "-Infinity", "1" + "0" * 400,
                      str(safety.NOTED_MAX + 1), '"soon"', "true", "null", "[1]"):
            with self.subTest(noted=noted[:12]):
                Path(self.path).write_text('{"holder": %s, "unresolved": [{"name": "%s", "noted": %s}, '
                                           '{"name": "%s", "noted": 5}]}' % (holder % 4321, name, noted, name))
                os.chmod(self.path, 0o600)
                recorded, entries = safety.read_ledger(self.path)
                self.assertEqual(entries, [{"name": name, "noted": 5}])  # Entry by entry: the good one stays.
                self.assertEqual(recorded["pid"], 4321)
        for pid in ("0", "1", "-7", str(safety.PID_MAX + 1), "1" + "0" * 30, "4321.0", '"4321"', "true"):
            with self.subTest(pid=pid[:12]):
                Path(self.path).write_text('{"holder": %s, "unresolved": []}' % (holder % pid))
                self.assertEqual(safety.read_ledger(self.path), (None, []))
        Path(self.path).write_text('{"holder": %s, "unresolved": []}' % (holder % safety.PID_MAX))
        self.assertEqual(safety.read_ledger(self.path)[0]["pid"], safety.PID_MAX)

    def test_reading_the_ledger_never_creates_or_locks_and_ignores_what_is_not_a_record(self):
        self.assertEqual(safety.read_ledger(self.path), (None, []))
        self.assertFalse(os.path.lexists(self.path))
        lock = safety.OperationLock(self.path).acquire("import")
        self.assertEqual(safety.read_ledger(self.path)[0]["pid"], os.getpid())  # Readable while it is held.
        lock.release()
        good = {"name": t.PROJECT + "-job-0123abcd", "noted": 5.0}
        for text, expected in (
                ("", []), ("not json", []), ("[1, 2]", []), ('{"unresolved": "x"}', []),
                (json.dumps({"unresolved": [good, {"name": "komika-server-1", "noted": 1}, {"name": t.PROJECT + "-job-0123abcd"},
                                            {"name": t.PROJECT + "-postgres", "noted": 1}, "x", None,
                                            {"name": t.PROJECT + "-meta-89abcdef", "noted": "soon"}],
                             "holder": {"pid": "1; rm -rf /", "subcommand": "up", "since": "now"}}), [good]),
                (json.dumps({"unresolved": [good]}) + "\ntrailing bytes of a longer earlier record}}", [good])):
            with self.subTest(text=text[:30]):
                Path(self.path).write_text(text)
                self.assertEqual(safety.read_ledger(self.path), (None, expected))

    def test_unresolved_creations_survive_the_holder_and_belong_to_their_project(self):
        other = "komika-pg-rehearsal-other-job-00000001"
        lock = safety.OperationLock(self.path).acquire("import")
        lock.note(t.PROJECT + "-job-0123abcd")
        lock.note(t.PROJECT + "-job-0123abcd")  # Once.
        lock.note(other)
        lock.release()
        holder, entries = safety.read_ledger(self.path)
        self.assertIsNone(holder)
        self.assertEqual([entry["name"] for entry in entries], [t.PROJECT + "-job-0123abcd", other])
        lock = safety.OperationLock(self.path).acquire("stop-source")
        self.assertEqual([entry["name"] for entry in lock.unresolved(t.PROJECT)], [t.PROJECT + "-job-0123abcd"])
        self.assertEqual(safety.unresolved_project(other), "komika-pg-rehearsal-other")
        lock.resolve([t.PROJECT + "-job-0123abcd"])
        self.assertEqual([entry["name"] for entry in lock.unresolved()], [other])
        lock.release()
        self.assertEqual([entry["name"] for entry in safety.read_ledger(self.path)[1]], [other])

    def test_the_fixed_path_is_outside_every_rehearsal_root_and_not_a_setting(self):
        self.assertIn(safety.LOCK_PATH, ("/run/lock/komika-pg-rehearsal.lock", "/private/tmp/komika-pg-rehearsal.lock"))
        source = (safety.KIT / "safety.py").read_text() + (safety.KIT / "rehearsal.py").read_text()
        self.assertNotIn("KOMIKA_REHEARSAL_LOCK", source)  # No variable or flag moves it.
        self.assertEqual(source.count("LOCK_PATH = "), 1)


MOUNTINFO = """\
25 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw
26 25 0:5 / /dev rw,nosuid shared:2 - devtmpfs udev rw
27 25 0:24 / /run rw,nosuid,nodev shared:5 - tmpfs tmpfs rw
40 25 8:16 / /mnt/with\\040space rw,relatime shared:30 master:1 - ext4 /dev/sdb rw
"""
ALIAS = "41 25 8:1 /var/lib/docker/volumes/komika_server-data/_data /srv/rehearsal-output rw,relatime shared:31 - ext4 /dev/sda1 rw\n"


class FakeStat:
    """Distinct inode per path; `alias` maps a path onto another path's inode."""
    def __init__(self, denied=(), missing=(), alias=None):
        self.denied, self.missing, self.alias, self.inodes = denied, missing, alias or {}, {}

    def __call__(self, path):
        path = self.alias.get(path, path)
        if any(path == d or path.startswith(d + "/") for d in self.denied):
            raise PermissionError(path)
        if any(path == m or path.startswith(m + "/") for m in self.missing):
            raise FileNotFoundError(path)
        inode = self.inodes.setdefault(path, len(self.inodes) + 100)
        return os.stat_result((stat_module.S_IFDIR | 0o700, inode, 2049, 2, 0, 0, 0, 0, 0, 0))


class FenceTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.base = Path(directory.name).resolve()
        self.uid = os.getuid()
        self.root = self.base / "rehearsal"
        self.root.mkdir(mode=0o700)
        (self.root / safety.ROOT_MARKER).write_text(safety.marker_text(self.uid))
        self.production = self.base / "production-volume"
        self.production.mkdir()
        self.docker = self.base / "docker-data"
        self.docker.mkdir()
        self.protected = [("Docker data root", str(self.docker)), ("volume komika_server-data", str(self.production))]

    def fence(self, root=None, protected=None, **options):
        options.setdefault("platform", "darwin")  # No mount table: path and inode checks only.
        options.setdefault("homes", [])
        return safety.host_fence(root or self.root, self.protected if protected is None else protected,
                                 uid=self.uid, **options)

    def test_marked_private_root_outside_docker_passes(self):
        self.assertIsNone(self.fence())

    def test_root_must_be_marked_private_and_caller_owned(self):
        (self.root / safety.ROOT_MARKER).unlink()
        with self.assertRaisesRegex(ValueError, "marker"):
            self.fence()
        (self.root / safety.ROOT_MARKER).write_text(safety.marker_text(self.uid + 1))
        with self.assertRaisesRegex(ValueError, "marker"):
            self.fence()
        (self.root / safety.ROOT_MARKER).unlink()
        (self.root / safety.ROOT_MARKER).symlink_to(self.base / "elsewhere")
        (self.base / "elsewhere").write_text(safety.marker_text(self.uid))
        with self.assertRaisesRegex(ValueError, "marker"):
            self.fence()
        (self.root / safety.ROOT_MARKER).unlink()
        (self.root / safety.ROOT_MARKER).write_text(safety.marker_text(self.uid))
        self.fence()
        self.root.chmod(0o755)
        with self.assertRaisesRegex(ValueError, "0700"):
            self.fence()
        self.root.chmod(0o700)
        with patch.object(safety.os, "getuid", return_value=self.uid + 1), self.assertRaisesRegex(ValueError, "0700"):
            safety.host_fence(self.root, self.protected, uid=self.uid + 1, platform="darwin", homes=[])

    def test_root_owned_root_without_marker_is_refused_for_a_root_caller(self):
        # The third check's case: caller is root, the directory is root-owned 0700.
        owned_by_root = os.stat_result((stat_module.S_IFDIR | 0o700, 7, 2049, 2, 0, 0, 0, 0, 0, 0))
        marker_by_root = os.stat_result((stat_module.S_IFREG | 0o600, 8, 2049, 1, 0, 0, 0, 0, 0, 0))
        (self.root / safety.ROOT_MARKER).unlink()

        def lstat(path):
            if path == str(self.root):
                return owned_by_root
            if not os.path.lexists(path):
                raise FileNotFoundError(path)
            return marker_by_root

        with self.assertRaisesRegex(ValueError, "marker"):
            safety.host_fence(self.root, self.protected, uid=0, lstat=lstat, platform="darwin", homes=[])
        (self.root / safety.ROOT_MARKER).write_text(safety.marker_text(0))
        safety.host_fence(self.root, self.protected, uid=0, lstat=lstat, platform="darwin", homes=[])

    def test_slash_home_relative_and_symlinked_roots_are_refused(self):
        with self.assertRaises(ValueError):
            self.fence(root=Path("/"))
        with self.assertRaisesRegex(ValueError, "home directory"):
            self.fence(homes=[str(self.root)])
        (self.base / "link").symlink_to(self.root)
        for root in (self.base / "link", Path("relative"), self.base / "missing"):
            with self.subTest(root=root), self.assertRaises((ValueError, OSError)):
                self.fence(root=root)

    def test_root_inside_or_around_docker_data_and_production_mounts_is_refused(self):
        inside = self.docker / "volumes" / "x"
        inside.mkdir(parents=True, mode=0o700)
        os.chmod(inside, 0o700)
        (inside / safety.ROOT_MARKER).write_text(safety.marker_text(self.uid))
        with self.assertRaisesRegex(ValueError, "Docker data root"):
            self.fence(root=inside)
        with self.assertRaisesRegex(ValueError, "overlaps"):
            self.fence(protected=[("container komika-server-1 mount", str(self.root / "sub"))])
        with self.assertRaisesRegex(ValueError, "overlaps"):
            self.fence(protected=[("container node-exporter mount", "/")])
        # A symlink that leads a production mount into the root is resolved first.
        (self.base / "alias").symlink_to(self.root)
        with self.assertRaisesRegex(ValueError, "overlaps"):
            self.fence(protected=[("container x mount", str(self.base / "alias" / "sub"))])
        # File mounts elsewhere (socket, localtime) do not block a rehearsal.
        self.fence(protected=[("container x mount", "/var/run/docker.sock"), ("container x mount", "/etc/localtime")])

    def test_bind_alias_of_a_production_volume_is_refused_by_inode(self):
        # /srv/rehearsal-output is a bind mount of the production volume: the same
        # (st_dev, st_ino) through another path. Path.resolve() cannot see it.
        output = self.base / "rehearsal-output"
        root = output / "rehearsal"
        root.mkdir(parents=True, mode=0o700)
        (root / safety.ROOT_MARKER).write_text(safety.marker_text(self.uid))
        self.fence(root=root)  # Control: without the alias this root is fine.
        with self.assertRaisesRegex(ValueError, "bind-mount alias"):
            self.fence(root=root, stat=FakeStat(alias={str(output): str(self.production)}))
        with self.assertRaisesRegex(ValueError, "bind-mount alias"):  # The root itself is the alias.
            self.fence(root=root, stat=FakeStat(alias={str(root): str(self.production)}))
        with self.assertRaisesRegex(ValueError, "bind-mount alias"):  # Docker's data root aliased.
            self.fence(root=root, stat=FakeStat(alias={str(output): str(self.docker)}))
        # Reverse direction: a production mount lives below an alias of the root.
        nested = self.production / "nested"
        with self.assertRaisesRegex(ValueError, "bind-mount alias"):
            self.fence(root=root, protected=[("container x mount", str(nested))],
                       stat=FakeStat(alias={str(self.production): str(root)}, missing=[str(nested)]))

    def test_bind_alias_is_refused_from_the_mount_table_without_any_permission(self):
        # A non-root caller cannot stat below /var/lib/docker; inodes cannot help.
        protected = [("Docker data root", "/var/lib/docker"),
                     ("volume komika_server-data", "/var/lib/docker/volumes/komika_server-data/_data")]
        denied = FakeStat(denied=["/var/lib/docker/volumes"])
        options = dict(uid=self.uid, prospective=True, stat=denied, platform="linux", homes=[])
        with patch.object(safety.os.path, "realpath", side_effect=lambda p: p):
            mount = safety.host_fence(Path("/srv/rehearsal-output/rehearsal"), protected, mountinfo=MOUNTINFO, **options)
            self.assertEqual(mount["point"], "/")  # Control: no alias line, accepted.
            with self.assertRaisesRegex(ValueError, "host mount table"):
                safety.host_fence(Path("/srv/rehearsal-output/rehearsal"), protected, mountinfo=MOUNTINFO + ALIAS, **options)
            with self.assertRaisesRegex(ValueError, "host mount table"):  # Root IS the mount point.
                safety.host_fence(Path("/srv/rehearsal-output"), protected, mountinfo=MOUNTINFO + ALIAS, **options)
            # A mount whose source lies anywhere inside Docker's data root.
            inside = "42 25 8:1 /var/lib/docker/overlay2/abc/merged /srv/layer rw - ext4 /dev/sda1 rw\n"
            with self.assertRaisesRegex(ValueError, "Docker data root"):
                safety.host_fence(Path("/srv/layer/rehearsal"), protected, mountinfo=MOUNTINFO + inside, **options)
            # Docker's data root is its own filesystem, and the root sits on that device elsewhere.
            own = "43 25 8:32 / /var/lib/docker rw - ext4 /dev/sdc rw\n44 25 8:32 /volumes/komika_server-data/_data /srv/out rw - ext4 /dev/sdc rw\n"
            with self.assertRaisesRegex(ValueError, "host mount table"):
                safety.host_fence(Path("/srv/out/rehearsal"), protected, mountinfo=MOUNTINFO + own, **options)
            safety.host_fence(Path("/srv/other/rehearsal"), protected, mountinfo=MOUNTINFO + own, **options)
            # A daemon whose data root this host cannot see is not a local daemon.
            with self.assertRaisesRegex(ValueError, "not visible"):
                safety.host_fence(Path("/srv/other/rehearsal"), protected, mountinfo=MOUNTINFO,
                                  **dict(options, stat=FakeStat(missing=["/var/lib/docker"])))

    def test_the_longest_mount_point_wins_whatever_the_order_of_the_table(self):
        # F11: neither "first match" nor "last match" is the mount that holds a path.
        table = safety.parse_mountinfo(
            "25 1 8:1 / / rw - ext4 /dev/sda1 rw\n"
            "30 25 8:48 /inner-root /data/inner rw - ext4 /dev/sdd rw\n"
            "31 25 8:32 /data-root /data rw - ext4 /dev/sdc rw\n"
            "32 25 8:64 / /data/inner/deep/er rw - ext4 /dev/sde rw\n"
            "33 25 8:16 / /dat rw - ext4 /dev/sdb rw\n")
        self.assertEqual(safety.backing("/data/inner/x", table)[1], ("8:48", "/inner-root/x"))
        self.assertEqual(safety.backing("/data/inner", table)[1], ("8:48", "/inner-root"))
        self.assertEqual(safety.backing("/data/other", table)[1], ("8:32", "/data-root/other"))
        self.assertEqual(safety.backing("/data/inner/deep/er/x", table)[1], ("8:64", "/x"))
        self.assertEqual(safety.backing("/data/inner/deep/erx", table)[1], ("8:48", "/inner-root/deep/erx"))
        self.assertEqual(safety.backing("/database", table)[1], ("8:1", "/database"))  # Not below /dat or /data.
        self.assertEqual(safety.backing("/dat/a", table)[1], ("8:16", "/a"))
        self.assertEqual(safety.backing("/", table)[1], ("8:1", "/"))
        # The same point twice: the later line shadows the earlier one.
        shadowed = safety.parse_mountinfo("25 1 8:1 / / rw - ext4 /dev/sda1 rw\n"
                                          "30 25 8:16 /old /srv rw - ext4 /dev/sdb rw\n31 25 8:32 /new /srv rw - ext4 /dev/sdc rw\n")
        self.assertEqual(safety.backing("/srv/x", shadowed)[1], ("8:32", "/new/x"))
        # The fence uses it: a root below the alias mount is refused although an
        # earlier, shorter mount of an unrelated filesystem also contains the path.
        protected = [("Docker data root", "/var/lib/docker"), ("volume komika_server-data", "/var/lib/docker/volumes/komika_server-data/_data")]
        alias = ("25 1 8:1 / / rw - ext4 /dev/sda1 rw\n"
                 "30 25 8:1 /var/lib/docker/volumes/komika_server-data/_data /srv/out/alias rw - ext4 /dev/sda1 rw\n"
                 "31 25 8:32 / /srv/out rw - ext4 /dev/sdc rw\n")
        options = dict(uid=os.getuid(), prospective=True, stat=FakeStat(denied=["/var/lib/docker/volumes"]), platform="linux", homes=[])
        with patch.object(safety.os.path, "realpath", side_effect=lambda p: p):
            with self.assertRaisesRegex(ValueError, "host mount table"):
                safety.host_fence(Path("/srv/out/alias/rehearsal"), protected, mountinfo=alias, **options)
            safety.host_fence(Path("/srv/out/elsewhere/rehearsal"), protected, mountinfo=alias, **options)

    def test_platform_and_mount_table_come_from_the_module_never_from_the_caller(self):
        self.assertEqual(safety.PLATFORM, __import__("sys").platform)
        # An injected Linux platform reads the injected table; nothing else changes the platform.
        with patch.object(safety, "PLATFORM", "linux"), patch.object(safety, "read_mountinfo", return_value=MOUNTINFO + ALIAS), \
                patch.object(safety.os.path, "realpath", side_effect=lambda p: p):
            options = dict(uid=self.uid, prospective=True, stat=FakeStat(denied=["/var/lib/docker/volumes"]), homes=[])
            protected = [("Docker data root", "/var/lib/docker"),
                         ("volume komika_server-data", "/var/lib/docker/volumes/komika_server-data/_data")]
            with self.assertRaisesRegex(ValueError, "host mount table"):
                safety.host_fence(Path("/srv/rehearsal-output/rehearsal"), protected, **options)
        with patch.object(safety, "PLATFORM", "darwin"):
            self.assertIsNone(safety.host_fence(self.root, self.protected, uid=self.uid, homes=[]))
        with patch.dict(os.environ, {"KOMIKA_REHEARSAL_PLATFORM": "darwin", "PLATFORM": "darwin"}):
            self.assertEqual(safety.Rehearsal(prepare=False).platform, __import__("sys").platform)

    def test_mount_table_parser_and_backing_paths(self):
        mounts = safety.parse_mountinfo(MOUNTINFO + ALIAS)
        self.assertEqual(mounts[3]["point"], "/mnt/with space")
        self.assertEqual(safety.backing("/mnt/with space/a/b", mounts)[1], ("8:16", "/a/b"))
        self.assertEqual(safety.backing("/srv/rehearsal-output/new", mounts)[1],
                         ("8:1", "/var/lib/docker/volumes/komika_server-data/_data/new"))
        self.assertEqual(safety.backing("/srv/rehearsal-output-other", mounts)[1], ("8:1", "/srv/rehearsal-output-other"))
        for text in ("", "garbage\n", "1 2 3 4 5 6\n"):
            with self.subTest(text=text), self.assertRaises(ValueError):
                safety.parse_mountinfo(text)

    def test_only_own_directories_below_the_root_are_exempt(self):
        own = {"Name": "/komika-pitr-0123456789ab", "Config": {"Labels": {t.LABEL: t.PROJECT}},
               "Mounts": [{"Source": str(self.root / "restore-1")}, {"Source": ""}]}
        mislabelled = {"Name": "/komika-server-1", "Config": {"Labels": {t.LABEL: t.PROJECT}},
                       "Mounts": [{"Source": str(self.production)}]}
        other = {"Name": "/other", "Config": {"Labels": {t.LABEL: "komika-pg-rehearsal-other"}},
                 "Mounts": [{"Source": str(self.root / "restore-2")}]}
        volumes = [{"Name": t.PROJECT + "-postgres-data", "Labels": {t.LABEL: t.PROJECT},
                    "Mountpoint": str(self.docker / "volumes" / "own")}]
        paths = safety.protected_paths(str(self.docker), [own, mislabelled, other], volumes, t.PROJECT, self.root)
        self.assertEqual([path for _, path in paths], [str(self.docker), str(self.docker / "volumes" / "own"),
                                                       str(self.production), str(self.root / "restore-2")])
        # Without a project (hosted recovery) nothing at all is exempt.
        self.assertEqual(len(safety.protected_paths(str(self.docker), [own, mislabelled, other], volumes)), 5)

    def test_subnet_conflicts_distinguish_own_and_foreign_networks(self):
        networks = [{"Name": "komika_komika", "Labels": {}, "IPAM": {"Config": [{"Subnet": "172.21.0.0/16"}, {"Subnet": "fd00:21::/64"}]}},
                    {"Name": t.PROJECT + "-network", "Labels": {t.LABEL: t.PROJECT}, "IPAM": {"Config": [{"Subnet": "172.30.21.0/24"}]}},
                    {"Name": "clubsite", "Labels": {}, "IPAM": {"Config": [{"Subnet": "172.29.0.0/16"}]}},
                    {"Name": "host", "Labels": None, "IPAM": {"Config": None}}]
        self.assertEqual(safety.subnet_conflicts(networks, t.PROJECT),
                         [("172.30.21.0/24", t.PROJECT + "-network", True), ("172.29.42.0/24", "clubsite", False)])


if __name__ == "__main__":
    unittest.main()
