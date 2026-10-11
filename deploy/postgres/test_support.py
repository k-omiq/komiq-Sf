"""Literal fixtures for the rehearsal tests; independent of the code under test.

RENDERED is `docker compose config --format json` of rehearsal-compose.yml as
printed by Docker Compose 5.1.4 (2026-10-11) with a synthetic env file
(fix round 4 defaults: 3 GiB and one CPU for PostgreSQL, TLS verification on).
CONTAINER is the non-secret shape of `docker container inspect` for the
postgres service that Compose created from it (Docker 29.5.2). Fix round 5
added the resource fields that the live check now compares (Memory,
MemorySwap, NanoCpus, CpuQuota, CpuPeriod, PidsLimit, ShmSize, LogConfig,
RestartPolicy) and the image's own environment names; their values are what
Docker reports for the limits of RENDERED. They were written from Docker's
documented inspect shape, not captured again from a daemon in round 5.
"""
import copy
import json

PROJECT = "komika-pg-rehearsal-test"
PASSWORD = "synthetic0synthetic0synthetic0"
SECRET = "syntheticR2secretvalue"
VALUES = {
    "KOMIKA_REHEARSAL_PROJECT": PROJECT,
    "KOMIKA_POSTGRES_PASSWORD": PASSWORD,
    "KOMIKA_PG_INIT_NEW": "1",
    "KOMIKA_R2_ENDPOINT": "example.r2.invalid",
    "KOMIKA_R2_BUCKET": "synthetic-bucket",
    "KOMIKA_R2_PREFIX": "rehearsal/test",
    "KOMIKA_PRODUCTION_R2_PREFIX": "komika",
    "KOMIKA_R2_KEY": "synthetickey",
    "KOMIKA_R2_SECRET": SECRET,
}
NO_REPO_VALUES = {"KOMIKA_REHEARSAL_PROJECT": PROJECT, "KOMIKA_POSTGRES_PASSWORD": PASSWORD,
                  "KOMIKA_REHEARSAL_NO_REPO": "1"}
IMAGE = "komika-postgres:18.6-pgbackrest-2.59"
LABEL = "cc.komiq.rehearsal"
# Docker's data root of the fixtures; a test with its own (temporary) root replaces it.
DOCKER_ROOT = "/var/lib/docker"

_RENDERED = json.loads(r'''
{
  "name": "komika-pg-rehearsal-test",
  "networks": {
    "rehearsal": {
      "name": "komika-pg-rehearsal-test-network",
      "driver": "bridge",
      "ipam": {"config": [{"subnet": "172.30.21.0/24"}]},
      "labels": {"cc.komiq.rehearsal": "komika-pg-rehearsal-test"}
    }
  },
  "services": {
    "postgres": {
      "cpus": 1,
      "command": null,
      "container_name": "komika-pg-rehearsal-test-postgres",
      "entrypoint": null,
      "environment": {
        "KOMIKA_PG_ALLOWED_CIDRS": "172.30.21.0/24",
        "KOMIKA_PG_INIT_NEW": "1",
        "KOMIKA_POSTGRES_PASSWORD": "synthetic0synthetic0synthetic0",
        "KOMIKA_PRODUCTION_R2_PREFIX": "komika",
        "KOMIKA_R2_BUCKET": "synthetic-bucket",
        "KOMIKA_R2_ENDPOINT": "example.r2.invalid",
        "KOMIKA_R2_KEY": "synthetickey",
        "KOMIKA_R2_PREFIX": "rehearsal/test",
        "KOMIKA_R2_SECRET": "syntheticR2secretvalue",
        "KOMIKA_R2_VERIFY_TLS": "1",
        "KOMIKA_REHEARSAL_NO_REPO": "0",
        "KOMIKA_REHEARSAL_PROJECT": "komika-pg-rehearsal-test",
        "PGDATA": "/var/lib/postgresql/18/docker"
      },
      "healthcheck": {
        "test": ["CMD", "/opt/komika-postgres/health.sh"],
        "timeout": "5s", "interval": "20s", "retries": 8, "start_period": "2m0s"
      },
      "image": "komika-postgres:18.6-pgbackrest-2.59",
      "init": true,
      "labels": {"cc.komiq.rehearsal": "komika-pg-rehearsal-test"},
      "logging": {"driver": "json-file", "options": {"max-file": "3", "max-size": "10m"}},
      "mem_limit": "3221225472",
      "memswap_limit": "3221225472",
      "networks": {"rehearsal": null},
      "pids_limit": 256,
      "restart": "no",
      "shm_size": "1073741824",
      "stop_grace_period": "4m0s",
      "tmpfs": ["/run/komika:mode=0700"],
      "volumes": [
        {"type": "volume", "source": "postgres-data", "target": "/var/lib/postgresql", "volume": {}},
        {"type": "volume", "source": "postgres-socket", "target": "/var/run/postgresql", "volume": {}}
      ]
    },
    "postgres-backup": {
      "cpus": 1,
      "command": ["scheduler"],
      "container_name": "komika-pg-rehearsal-test-postgres-backup",
      "depends_on": {"postgres": {"condition": "service_healthy", "required": true}},
      "entrypoint": null,
      "environment": {
        "KOMIKA_PG_ALLOWED_CIDRS": "172.30.21.0/24",
        "KOMIKA_PG_INIT_NEW": "1",
        "KOMIKA_POSTGRES_PASSWORD": "synthetic0synthetic0synthetic0",
        "KOMIKA_PRODUCTION_R2_PREFIX": "komika",
        "KOMIKA_R2_BUCKET": "synthetic-bucket",
        "KOMIKA_R2_ENDPOINT": "example.r2.invalid",
        "KOMIKA_R2_KEY": "synthetickey",
        "KOMIKA_R2_PREFIX": "rehearsal/test",
        "KOMIKA_R2_SECRET": "syntheticR2secretvalue",
        "KOMIKA_R2_VERIFY_TLS": "1",
        "KOMIKA_REHEARSAL_NO_REPO": "0",
        "KOMIKA_REHEARSAL_PROJECT": "komika-pg-rehearsal-test",
        "PGDATA": "/var/lib/postgresql/18/docker"
      },
      "healthcheck": {
        "test": ["CMD", "python3", "/opt/komika-postgres/hosted.py", "health"],
        "timeout": "5s", "interval": "1m0s", "retries": 3, "start_period": "10m0s"
      },
      "image": "komika-postgres:18.6-pgbackrest-2.59",
      "init": true,
      "labels": {"cc.komiq.rehearsal": "komika-pg-rehearsal-test"},
      "logging": {"driver": "json-file", "options": {"max-file": "3", "max-size": "10m"}},
      "mem_limit": "536870912",
      "memswap_limit": "536870912",
      "networks": {"rehearsal": null},
      "pids_limit": 128,
      "restart": "no",
      "stop_grace_period": "4m0s",
      "tmpfs": ["/run/komika:mode=0700"],
      "volumes": [
        {"type": "volume", "source": "postgres-data", "target": "/var/lib/postgresql", "volume": {}},
        {"type": "volume", "source": "postgres-socket", "target": "/var/run/postgresql", "volume": {}}
      ]
    }
  },
  "volumes": {
    "postgres-data": {"name": "komika-pg-rehearsal-test-postgres-data", "driver": "local",
                      "labels": {"cc.komiq.rehearsal": "komika-pg-rehearsal-test"}},
    "postgres-socket": {"name": "komika-pg-rehearsal-test-postgres-socket", "driver": "local",
                        "labels": {"cc.komiq.rehearsal": "komika-pg-rehearsal-test"}}
  }
}
''')


def rendered(no_repo=False):
    """A fresh copy of the literal rendering (optionally the no-repository one)."""
    model = copy.deepcopy(_RENDERED)
    if no_repo:
        for service in model["services"].values():
            for key in ("KOMIKA_R2_ENDPOINT", "KOMIKA_R2_BUCKET", "KOMIKA_R2_PREFIX",
                        "KOMIKA_PRODUCTION_R2_PREFIX", "KOMIKA_R2_KEY", "KOMIKA_R2_SECRET"):
                service["environment"][key] = ""
            service["environment"]["KOMIKA_REHEARSAL_NO_REPO"] = "1"
            service["environment"]["KOMIKA_PG_INIT_NEW"] = "0"
    return model


def container(service="postgres", running=True, health="healthy", no_repo=False):
    """Literal inspect shape of a Compose-created rehearsal container."""
    name = f"{PROJECT}-{service}"
    environment = rendered(no_repo)["services"][service]["environment"]
    memory = 3221225472 if service == "postgres" else 536870912
    return {
        "Id": ("a" if service == "postgres" else "b") * 64,
        "Image": "sha256:fake",
        "Name": "/" + name,
        "RestartCount": 0,
        "State": {"Status": "running" if running else "exited", "Running": running, "OOMKilled": False,
                  "StartedAt": "2026-10-11T12:27:28.246046331Z", "Health": {"Status": health}},
        "Config": {
            "User": "", "Image": IMAGE,
            "Entrypoint": ["/opt/komika-postgres/entrypoint.sh"],
            "Cmd": ["postgres"] if service == "postgres" else ["scheduler"],
            "Env": [f"{key}={value}" for key, value in environment.items()]
                   + ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/usr/lib/postgresql/18/bin",
                      "GOSU_VERSION=1.19", "LANG=en_US.utf8", "PG_MAJOR=18", "PG_VERSION=18.6-1.pgdg12+1",
                      "PGBACKREST_CONFIG=/run/komika/pgbackrest.conf"],
            "Labels": {LABEL: PROJECT, "com.docker.compose.project": PROJECT,
                       "com.docker.compose.service": service, "com.docker.compose.oneoff": "False"},
        },
        "HostConfig": {
            "Privileged": False, "NetworkMode": PROJECT + "-network", "PortBindings": {},
            "PublishAllPorts": False, "CapAdd": None, "Devices": None, "DeviceRequests": None,
            "VolumesFrom": None, "SecurityOpt": None,
            "Binds": [f"{PROJECT}-postgres-data:/var/lib/postgresql:rw",
                      f"{PROJECT}-postgres-socket:/var/run/postgresql:rw"],
            "PidMode": "", "UTSMode": "", "UsernsMode": "", "IpcMode": "private",
            "Tmpfs": {"/run/komika": "mode=0700"}, "Mounts": None, "CgroupnsMode": "private",
            "Memory": memory, "MemorySwap": memory, "MemoryReservation": 0, "NanoCpus": 1000000000,
            "CpuQuota": 0, "CpuPeriod": 0, "CpuShares": 0, "PidsLimit": 256 if service == "postgres" else 128,
            "ShmSize": 1073741824 if service == "postgres" else 67108864,
            "LogConfig": {"Type": "json-file", "Config": {"max-file": "3", "max-size": "10m"}},
            "RestartPolicy": {"Name": "no", "MaximumRetryCount": 0},
        },
        "Mounts": [
            {"Type": "volume", "Name": f"{PROJECT}-postgres-data",
             "Source": f"{DOCKER_ROOT}/volumes/{PROJECT}-postgres-data/_data",
             "Destination": "/var/lib/postgresql", "Driver": "local", "Mode": "rw", "RW": True, "Propagation": ""},
            {"Type": "volume", "Name": f"{PROJECT}-postgres-socket",
             "Source": f"{DOCKER_ROOT}/volumes/{PROJECT}-postgres-socket/_data",
             "Destination": "/var/run/postgresql", "Driver": "local", "Mode": "rw", "RW": True, "Propagation": ""},
        ],
        "NetworkSettings": {"Ports": {"5432/tcp": None}, "Networks": {PROJECT + "-network": {}}},
    }
