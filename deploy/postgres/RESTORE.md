# Hosted PostgreSQL backup and recovery

This package backs up and restores the production PostgreSQL cluster, which has run
on it since the cutover on 2026-10-13. Phase 6's gate of two clean rehearsals in a row
was waived by the owner, not passed: rehearsal 1 was clean, rehearsal 2 was ended
early. A restore or point-in-time drill against the live post-cutover repository has not
been recorded yet. Use PostgreSQL 18.6 and this image's pgBackRest 2.59.3 for every
restore. Keep existing PostgreSQL volumes until you have verified the restored data.

There are two separate paths:

- A **rehearsal** (next section) uses only this directory. Copy ONLY
  `deploy/postgres` to the host, change into the copy and run every command
  from there. Nothing else of the repository belongs on a rehearsal host: the
  hosted Compose file names the production project `komika`, and `deploy.sh`
  can replace and remove hosted resources.
- **Hosted recovery** (Phase 6 and later, separate operator authorization) uses
  the hosted checkout. Its commands below name that checkout through one
  variable with an absolute path; they are never run on a rehearsal copy.

## Isolated database rehearsal

`./rehearsal.sh` is the only way to run a rehearsal. It accepts a fixed list of
subcommands and builds every Docker command line itself. No argument is
forwarded to Docker, Compose or pgBackRest: there is no way to name a service,
container, volume, mount, user, entrypoint, Compose file, project or repository
path. Anything outside the list is a usage error (exit status 2) before Docker
is contacted. `restore.sh`, `pitr-drill.sh`, `drill.py` and `deploy.sh` refuse
every rehearsal selection. Never run `docker compose` on
`rehearsal-compose.yml` yourself.

On a host that also runs production the order is: measure first, provision
second, run third. `rehearsal.sh` never builds and never pulls. Nothing in
this runbook builds an image on that host, prunes Docker resources, copies
with `--delete` or removes anything by a pattern. The complete sequence, ready
to paste, is "Pre-flight on the host" and "Full drill order" below. Three
things must exist before it.

**The image, built and saved off the host.** Build on a Linux machine with the
host's CPU architecture, from the reviewed tree, and save the image, the kit
as files with their hashes:

```sh
# On the BUILD machine, never on a host that runs production. `out` is a new directory.
out=/absolute/new/komika-pg-phase5a-r5-artifacts
mkdir -m 0700 -- "$out"
cd /absolute/reviewed/checkout
uname -m                                   # must equal `uname -m` on the host
docker build -t komika-postgres:18.6-pgbackrest-2.59 deploy/postgres
docker image inspect --format '{{.Id}}' komika-postgres:18.6-pgbackrest-2.59 > "$out/image-id.txt"
docker save -o "$out/komika-postgres-image.tar" komika-postgres:18.6-pgbackrest-2.59
gzip -- "$out/komika-postgres-image.tar"
tar -C deploy -czf "$out/komika-postgres-kit.tar.gz" --exclude=__pycache__ --exclude='test_*.py' --exclude=tests.sh --exclude=image-test.sh postgres
(cd -- "$out" && sha256sum komika-postgres-image.tar.gz komika-postgres-kit.tar.gz image-id.txt > SHA256SUMS && sha256sum SHA256SUMS)
```

Write down the last line (the hash of `SHA256SUMS`) where the host cannot
change it. Run the image tests on the build machine before anything is
copied (`sh deploy/postgres/image-test.sh`, see `deploy/README.md`).

The hosted Compose file uses the same tag. On a host that already runs hosted
PostgreSQL, loading another build under this tag changes what the next hosted
`up` starts, so the pre-flight first checks that the tag is absent on the
host. `preflight`, `up` and every subcommand that starts a container from the
image first compare the SHA-256 of each kit file the image carries
(`entrypoint.sh`, `pgbackrest.sh`, `health.sh`, `backup.sh`, `hosted.py`,
`safety.py`, `postgresql.conf`) with the file in the kit directory, in a
container without network, mount or privilege, limited to 64 MiB, half a CPU
and 32 processes. Any difference is a refusal that says to rebuild; `up` also
refuses a container that still runs an older build. This comparison proves
that those seven files are the same. It does not prove which base image, which
packages or which executables the image holds: that is what the hash of the
saved image file is for.

Keep the image file off the host for good. The Dockerfile installs one exact
pgBackRest package (`2.59.3-1.pgdg12+1`), and the PGDG repository offers only
its newest version: once PGDG moves on, the Dockerfile no longer builds, and a
restore still needs this pgBackRest series. On any host the file is loaded
with `gzip -dc FILE | docker load`.

**The kit, in a new directory.** Only `deploy/postgres` is unpacked on the
host, into a directory that did not exist before, from the hashed archive. No
`rsync --delete`, no reuse of an older kit directory.

**The private env file.** Write it as a regular file, owned by you, mode 0600, with LF line
endings. It is the only source of settings and credentials. Only plain
`KEY=VALUE` lines, blank lines and `#` comment lines are accepted: no quotes, no
`export`, no interpolation, no unknown or repeated key. The accepted keys are:

- `KOMIKA_REHEARSAL_PROJECT=komika-pg-rehearsal-phase6-r1` (must equal the
  exported value; `komika-pg-rehearsal-` plus 1 to 24 lower-case letters,
  digits and inner dashes).
- `KOMIKA_POSTGRES_PASSWORD` (24 to 128 URI-safe characters, new for this
  rehearsal, never production's).
- `KOMIKA_PG_INIT_NEW=1` for the first `up` on an empty rehearsal prefix; set it
  back to `0` afterwards. A later `up` after that change recreates the
  postgres container, because its environment changed: the data volume stays,
  PostgreSQL restarts once. A rehearsal `up` never restores: with an empty or
  absent data volume and a rehearsal prefix that already holds a backup, `up`
  is refused, whatever this key says, and the refusal names the explicit path
  (`restore --target NEWDIR --json-out FILE`). The hosted path is different
  and unchanged: there an empty data volume restores the latest backup.
- `KOMIKA_R2_ENDPOINT`, `KOMIKA_R2_BUCKET`, `KOMIKA_R2_KEY`, `KOMIKA_R2_SECRET`.
- `KOMIKA_R2_PREFIX=rehearsal/phase6-r1` (must start with `rehearsal/`).
- `KOMIKA_PRODUCTION_R2_PREFIX`: the prefix production really uses. It is not a
  secret. Both prefixes are canonicalized (surrounding and repeated slashes
  collapse; dot segments and empty prefixes are refused); equality and ancestry
  in either direction are refused. The tooling cannot know production's prefix:
  you must copy it from production's own backup configuration.
- `KOMIKA_REHEARSAL_MIN_FREE_GB` (optional integer from 2 to 64, default 3):
  the free space that the estimate of every writing step must leave on each
  filesystem it writes to. See "Disk space" below.
- Optional limits, each only up to its ceiling. PostgreSQL:
  `KOMIKA_REHEARSAL_PG_MEMORY` (default `3g`, ceiling `4g`), `_PG_SHM` (`1g`),
  `_PG_CPUS` (default `1`, ceiling `2`), `_PG_PIDS` (`256`). Backup scheduler:
  `KOMIKA_REHEARSAL_BACKUP_MEMORY` (`512m`), `_BACKUP_CPUS` (`1`),
  `_BACKUP_PIDS` (`128`). Throwaway restore: `KOMIKA_RESTORE_MEMORY` (default
  `3g`, ceiling `4g`), `KOMIKA_RESTORE_CPUS` (default `1`, ceiling `2`),
  `KOMIKA_RESTORE_PIDS` (default `128`, ceiling `256`). Memory also caps swap.
  PostgreSQL keeps its 1 GiB shared buffers, so leave headroom above that; the
  throwaway restore uses 256 MiB shared buffers. Block I/O is NOT limited (see
  "What the tooling cannot check").
- `KOMIKA_R2_VERIFY_TLS=0`: never on a real host. See "Local drill against a
  throwaway S3 store" below; every other use is refused.

Use a fresh shell and export exactly three variables. Any other
`KOMIKA_REHEARSAL_*`, `KOMIKA_R2_*`, `KOMIKA_RESTORE_*`, `KOMIKA_COMPOSE_*`,
`PGBACKREST_*`, `DOCKER_*` or `COMPOSE_*` variable in the environment is
refused. On Linux the Docker context must be `default` with a local Unix socket.

```sh
cd /absolute/new/komika-postgres-kit
export KOMIKA_REHEARSAL_PROJECT=komika-pg-rehearsal-phase6-r1
export KOMIKA_COMPOSE_ENV_FILE=/absolute/private/komika-rehearsal.env
export KOMIKA_REHEARSAL_ROOT=/absolute/new/rehearsal-output
r() { ./rehearsal.sh "$@"; }
```

**One operation at a time.** Every subcommand except `status`, `logs`,
`psql-check` and `archive-status` takes one host-wide lock before it validates
or measures anything, and holds it until the step and its cleanup have ended.
The lock is the fixed file `/run/lock/komika-pg-rehearsal.lock` (no variable or
flag moves it; it is outside every rehearsal root). The first caller creates it
with mode 0600; it must stay that caller's own regular file with one name, in
a directory that only root or the caller can rearrange, or every step is
refused. On the host that directory is `/run/lock`, owned by root and sticky
(`drwxrwxrwt`, as on Debian and Ubuntu). When it is missing, not root's, not
sticky although others may write to it, or not writable for the caller, the
refusal says exactly which of these it is; the kit never creates or changes
the directory. A second invocation, of any project, does not wait: it fails at once
with the holder's process ID, subcommand and start time, before it contacts
Docker. Two steps can therefore never be admitted against the same free
bytes. The kernel releases the lock when the process ends, however it ends, so
there is no stale lock to clear. Run every rehearsal command on a host as one
user. The lock serializes this kit only: it does not stop another
administrator or production itself from using memory or disk.

`KOMIKA_REHEARSAL_ROOT` must not exist yet: `preflight --init-root` creates it
(mode 0700, owned by the caller) with a marker file, and every later subcommand
requires that marker. Its parent must exist. A subcommand that needs the root
before it exists says so and names `preflight --init-root`; a missing env
file is named with its path. Every path given to the tooling must be absolute
and already resolved (no symlink, `.` or `..` component) and
use only letters, digits and `_.@+-`. The root may not be `/`, a home
directory, inside Docker's data root, inside or above any volume mountpoint or
any bind-mount source of a container that is not this rehearsal's, and it may
not be the same directory as any of those under another name: the check
compares device and inode numbers of the root and its ancestors and, on Linux,
the backing source of the root's mount in `/proc/self/mountinfo`. A host
container that bind-mounts `/` or an ancestor of the root (some monitoring
agents do) therefore blocks the rehearsal; choose a root that no container
sees. Every output file (`--out`, `--json-out`) and every restore
target (`--target`) must be a new name directly under the root.

Before every subcommand the tooling checks the SHA-256 of
`rehearsal-compose.yml` against the value pinned in `safety.py`, renders the
effective configuration with `docker compose config --format json` and compares
it with an exact expectation: two services, the image, entrypoint and command,
the full environment, the health check commands, the logging driver and its
options, the two labelled local volumes, one labelled bridge on
`172.30.21.0/24`, limits, no ports, no bind mounts, no host namespaces, no
privileges, no extra file, profile or network. Any key the expectation does not
know is refused. Existing resources with the project's names must carry
`cc.komiq.rehearsal=$KOMIKA_REHEARSAL_PROJECT`.

A label says who created a resource, not what it is backed by. So before
Compose starts, and before any use of an existing source volume, both volumes
are inspected: the driver must be `local`, there must be no driver options (a
`device` option can make any directory of the host a volume's storage), the
mountpoint must be exactly `<DockerRootDir>/volumes/<name>/_data`, on Linux
nothing may be mounted at or above that directory below Docker's data root,
and no container other than the project's two services may mount the volume
or any directory that overlaps it. The same check runs again on the volumes
that `up` has just created. A volume or container that fails is "not
rehearsal storage": every subcommand refuses, `stop-source` and `down` still
stop and remove the containers and report it, and no subcommand removes such
a volume. A host container that bind-mounts `/`, Docker's data root or its
`volumes` directory (some monitoring agents do) overlaps every volume and
therefore blocks the rehearsal.

Before every `docker exec`, and after `up`, the running container is inspected
and compared with the validated model, not only with the rendered file: image,
entrypoint, command, user, network, no ports, capabilities, devices or host
namespaces; the two mounts with their real source directories; the memory
limit, the swap limit, the CPU limit (`NanoCpus`, or quota and period), the
PID limit, the shm size, the log driver with `max-size` and `max-file`, and
restart policy `no`. Its environment must be exactly the model's names and
values plus the six names the image itself sets (`PATH`, `LANG`,
`GOSU_VERSION`, `PG_MAJOR`, `PG_VERSION`, `PGBACKREST_CONFIG`): any other
name, for example `PGHOST`, `PGPASSWORD`, a `PGBACKREST_*` or a `KOMIKA_*`
variable, is a refusal that prints the name and never the value. A container
whose limits were removed or changed with `docker update` is refused until
`stop-source` and `up` recreate it.

### Disk space

Limits cap memory, CPU and processes; the disk is shared with everything else
on the host. Each step that writes measures, while it holds the operation
lock, the free bytes an unprivileged writer may still use on the filesystem of
Docker's data root (the volumes) and on the filesystem of the rehearsal root
(restore targets, work directories), and refuses when its estimate of the
step would leave less than the floor (`KOMIKA_REHEARSAL_MIN_FREE_GB`, default
3 GiB). Every refusal prints the needed and the available bytes; every pass
prints one `disk_gate` line per filesystem.

| Step | Estimated need on Docker's data root | Estimated need on the rehearsal root |
| --- | --- | --- |
| `up`, `backup` | floor | floor |
| `restore` | floor | floor + 1.3 x the database size of the backup it will use (`backup[].info.size` of `pgbackrest info`) |
| `drill` | floor | floor + 1.3 x that size, while the source still fills the disk |

When both are one filesystem the two needs are added to one floor.

What this gate is and is not. It is an estimate, measured once before the
step. It is not a reservation and not a guarantee that the floor holds:

- WAL that a restore recovers, temporary files of a job and Docker's own
  metadata are not in the estimate.
- Two kit steps cannot run at once (the operation lock), but production and
  other administrators can take the space while a step runs.
- `up` writes only what a new empty cluster needs. It never restores in a
  rehearsal, so no step fills a volume with a backup behind a floor-only check.

So watch the disk yourself: `status` prints `watch-free` lines (free bytes and
the floor of both filesystems), `preflight` prints the gate and its rules, and
the pre-flight below defines `after`, which prints both filesystems after
every step, and a watch loop for the two long steps with the abort criteria
("While `restore` runs"). A `drill`, or a `restore` before the source volumes are gone,
needs source and target at once; when that does not fit, the refusal names the
order that does: `stop-source`, `destroy-source-volumes
--yes-delete-rehearsal-volumes`, then `restore --target-time`.

### Pre-flight on the host

Work in one terminal multiplexer session for the whole run, so that a lost
SSH connection does not end a step (a lost terminal sends SIGHUP; the kit
then unwinds cleanly, but the active step is interrupted):

```sh
command -v tmux && tmux new-session -s komika-pg-phase5a-r5
```

Reattach after a lost connection with `tmux attach -t komika-pg-phase5a-r5`.
If the host has neither `tmux` nor `screen`, stop and decide with the host's
owner; this runbook installs nothing on the host. Everything below runs in
the first window of that session (bash); the watch loop of "While `restore` runs" gets a second window.

**Paste one block, read what it printed, then paste the next.** Every block
is a function that runs when its last line is pasted. A check that fails
prints a line that starts with `STOP`, the function returns non-zero and the
last line of the block prints `gate=` without that block's letter. Each
block refuses to run unless the block before it passed in the same shell, so
several blocks pasted at once stop at the first failure too. The functions
check only what a command can decide; what you must read yourself is listed
after each block. Blocks A and B write nothing outside the one new directory
that B creates. The example names the run `phase5a-r5`; change only that
name.

```sh
# A. Inventory and measurements. Nothing here writes.
run=phase5a-r5
gate=
overlapping() {
  # Prints each mount source that is /, or holds Docker's volumes directory or the rehearsal's directory.
  while IFS= read -r source; do
    case "$source" in
      "") ;;
      /) echo "$source" ;;
      *) case "$docker_root/volumes/" in "$source"/*) echo "$source" ;; esac
         case "/srv/komika-pg-$run/" in "$source"/*) echo "$source" ;; esac ;;
    esac
  done
}
preflight_a() {
  stop=0
  id
  uname -m
  docker context show
  docker context inspect default --format '{{.Endpoints.docker.Host}}'
  docker compose version
  docker_root=$(docker info --format '{{.DockerRootDir}}')
  printf 'docker_root=%s\n' "$docker_root"
  awk '/MemTotal:|MemAvailable:|SwapFree:/{print}' /proc/meminfo
  docker stats --no-stream --format '{{.Name}} {{.MemUsage}} {{.CPUPerc}} {{.PIDs}}'
  docker ps -a --format '{{.ID}} {{.Names}} {{.Status}} {{.Label "com.docker.compose.project"}} {{.Label "cc.komiq.rehearsal"}}'
  docker ps -aq | xargs -r docker inspect --format '{{.Name}}{{range .Mounts}} {{.Type}}:{{.Source}}{{end}}'
  docker volume ls -q | xargs -r docker volume inspect --format '{{.Name}} {{.Driver}} {{.Mountpoint}} {{json .Options}} {{json .Labels}}'
  docker network ls -q | xargs -r docker network inspect --format '{{.Name}} {{.Driver}} {{json .IPAM.Config}} {{json .Labels}}'
  df -B1 -- "$docker_root" /srv
  findmnt -T "$docker_root" -o TARGET,SOURCE,FSROOT,FSTYPE,OPTIONS
  findmnt -T /srv -o TARGET,SOURCE,FSROOT,FSTYPE,OPTIONS
  ls -ld -- /run/lock
  test -z "${DOCKER_HOST-}${DOCKER_CONTEXT-}" || { echo "STOP: DOCKER_HOST or DOCKER_CONTEXT is set in this shell; the daemon must be the host's default one"; stop=1; }
  test "$(docker context show)" = default || { echo "STOP: the Docker context is not default"; stop=1; }
  case "$(docker context inspect default --format '{{.Endpoints.docker.Host}}')" in
    unix://*) ;;
    *) echo "STOP: Docker is not on a local Unix socket"; stop=1 ;;
  esac
  case "$docker_root" in
    /?*) ;;
    *) echo "STOP: Docker's data root is unknown"; stop=1 ;;
  esac
  if docker image inspect --format '{{.Id}}' komika-postgres:18.6-pgbackrest-2.59 > /dev/null 2>&1
  then echo "STOP: the image tag komika-postgres:18.6-pgbackrest-2.59 exists on this host already"; stop=1
  fi
  found=$(docker ps -aq --filter label=cc.komiq.rehearsal; docker volume ls -q --filter label=cc.komiq.rehearsal; docker network ls -q --filter label=cc.komiq.rehearsal)
  test -z "$found" || { echo "STOP: a container, volume or network carries a cc.komiq.rehearsal label already"; stop=1; }
  found=$( { docker ps -a --format '{{.Names}}'; docker volume ls --format '{{.Name}}'; docker network ls --format '{{.Name}}'; } | grep -E "^(komika-pg-rehearsal-$run|komika-pitr-)")
  test -z "$found" || { echo "STOP: these names exist already:"; echo "$found"; stop=1; }
  found=$(docker network ls -q | xargs -r docker network inspect --format '{{.Name}}{{range .IPAM.Config}} {{.Subnet}}{{end}}' | grep -E ' (172\.30\.21\.0/24|172\.29\.42\.0/24)( |$)')
  test -z "$found" || { echo "STOP: a network uses a subnet of the rehearsal already:"; echo "$found"; stop=1; }
  found=$(docker ps -aq | xargs -r docker inspect --format '{{range .Mounts}}{{println .Source}}{{end}}' | overlapping | sort -u)
  test -z "$found" || { echo "STOP: a container mounts a directory that holds Docker's volumes or the rehearsal's directory:"; echo "$found"; stop=1; }
  test "$(stat -c '%u %A' -- /run/lock)" = "0 drwxrwxrwt" || { echo "STOP: /run/lock is not a root-owned sticky directory (drwxrwxrwt)"; stop=1; }
  test "$stop" = 0 && echo "BLOCK A OK"
}
preflight_a && gate=A; echo "gate=$gate (A expected)"
```

Read as well, or stop here: `uname -m` equals the build machine's; the
memory, the `docker stats` lines and the free bytes are what you expect of
this host; no volume has driver options you do not know (if a volume of
another project shows options with credentials, do not copy that output
anywhere). Free space on Docker's data root, measured now, must cover
everything that is still to come: the loaded image (block C checks its
uncompressed size), the rehearsal dataset and its PostgreSQL storage, and the
3 GiB floor. If the
daemon keeps images on another filesystem (a separate containerd store),
measure that one too. Available memory must cover the rehearsal's limits
(3 GiB PostgreSQL, 3 GiB restore; the restore runs after the
source is gone) beside production's current use. What a `STOP` line of this
block means: `DOCKER_HOST` or `DOCKER_CONTEXT` is set in the shell (another
socket would pass the next test, and block C loads the image before block D
looks at the environment); the context is not `default` on a local Unix
socket; a
container, volume or network carries a `cc.komiq.rehearsal` label or a name
that starts with `komika-pg-rehearsal-phase5a-r5` or `komika-pitr-`; a
network uses `172.30.21.0/24` or `172.29.42.0/24`; a container mounts `/`,
`/srv`, the rehearsal's own directory, Docker's data root, a parent of it or
its `volumes` directory; the image tag is present already; or `/run/lock` is
not a root-owned sticky directory (the kit then refuses every step and says
which of the three it is: missing, not root's, or not sticky).

The image tag after an earlier run. A run of this runbook leaves its loaded
image on the host, so block A of the next run stops on the tag. This is the
one case in which the tag may be removed by hand, and only while ALL of this
holds; when one point is in doubt, stop and use another host or ask the
host's owner:

- The host does not run hosted PostgreSQL and no deployment on it names this
  tag: no Compose file of production on the host contains
  `komika-postgres:18.6-pgbackrest-2.59` (look in production's own deploy
  directory with `grep -rl`), and no volume of a hosted PostgreSQL exists. A
  hosted stack that is down, with its containers removed, would pass the
  container check below and would start a different image at its next `up`:
  after Phase 6 this exception is never used on the production host.
- The operator window of your own earlier run on this host shows the load
  (`Loaded image: komika-postgres:18.6-pgbackrest-2.59`).
- `docker image inspect --format '{{.Id}}'
  komika-postgres:18.6-pgbackrest-2.59` prints the ID in `image-id.txt` of
  that run. Read that file on the BUILD machine (`$out/image-id.txt`); the
  host's copy went with the run's directory.
- `docker ps -a --filter ancestor=komika-postgres:18.6-pgbackrest-2.59
  --format '{{.Names}}'` prints no line.

Then remove that one tag by its name (`docker image rm
komika-postgres:18.6-pgbackrest-2.59`), paste block A again, and let block C
load the image from the checked tarball. This step is prose: no block checks
it for you.

Block B and the removal of the restored directory use `sudo`. Run this line
alone first and answer its prompt, if there is one: a password prompt in the
middle of a pasted block would read the next pasted line as the password.
Block B checks with `sudo -n true` that no prompt can come and stops
otherwise. The line asks only when a password is needed: on a host whose
account has a password-free rule beside a rule with a password (the usual
cloud image), a bare `sudo -v` asks for a password that the account does
not have and waits five minutes.

```sh
sudo -n true || sudo -v
```

```sh
# B. The names of this run, its one new directory, the baseline.
others() {
  docker ps -aq --no-trunc | xargs -r docker inspect --format '{{.Id}} {{.Name}} {{.State.StartedAt}} {{.RestartCount}} {{.State.Status}} {{with index .Config.Labels "cc.komiq.rehearsal"}}{{.}}{{end}}' | awk 'NF == 5' | sort
}
after() {
  date -u +%Y-%m-%dT%H:%M:%SZ
  df -B1 -- "$docker_root" "$base"
  awk '/MemAvailable:/{print}' /proc/meminfo
  test -s "$records/others-before.txt" || { echo "STOP after $1: the baseline $records/others-before.txt is missing or empty; nothing can be compared"; return 1; }
  others > "$records/others-after-$1.txt"
  if cmp -s -- "$records/others-before.txt" "$records/others-after-$1.txt"
  then echo "OK after $1: no container outside the rehearsal changed"
  else echo "STOP after $1: a container outside the rehearsal changed"; diff -- "$records/others-before.txt" "$records/others-after-$1.txt"; return 1
  fi
}
preflight_b() {
  test "$gate" = A || { echo "STOP: block A has not passed in this shell"; return 1; }
  base=/srv/komika-pg-$run
  kit=$base/kit
  root=$base/root
  records=$base/records
  env_file=$base/private/rehearsal.env
  image=komika-postgres:18.6-pgbackrest-2.59
  made=
  test ! -e "$base" && test ! -L "$base" && echo "base is new: $base" \
    && sudo -n true \
    && sudo install -d -m 0700 -o "$(id -u)" -g "$(id -g)" -- "$base" && made=1 \
    && mkdir -m 0700 -- "$base/transfer" "$kit" "$base/input" "$records" "$base/private" \
    || {
      if test -n "$made"
      then echo "STOP: $base was created by this block, but not all of its subdirectories; it holds no data. Look at it (ls -la) and decide before anything else"
      else echo "STOP: $base exists already, or sudo needs a password (run 'sudo -v' alone first) or failed; no existing directory was changed"
      fi
      return 1
    }
  others > "$records/others-before.txt" && test -s "$records/others-before.txt" || { echo "STOP: the baseline could not be written or is empty"; return 1; }
  cat -- "$records/others-before.txt"
  echo "BLOCK B OK"
}
preflight_b && gate=B; echo "gate=$gate (B expected)"
```

`sudo install -d` runs only when the two tests before it have passed and
`sudo` needs no password: an existing directory of an earlier run is never
re-owned or re-used. The two `STOP` texts of that step say which case it
was: nothing was touched, or the new directory exists without all of its
subdirectories (then it is this run's and empty; do not paste B again
before you have looked at it). `others` is the baseline: ID, name, start
time, restart count and state of every container that does not carry a
rehearsal label (production and everything else on the host), without any
environment data. The baseline must not be empty: an empty file would make
every later comparison pass without comparing anything, so block B stops
when no other container is listed (on a host that runs production there
always is one; an empty list there means that the `docker inspect` line did
not print what this runbook expects). `after NAME` prints the time, the
free bytes of both filesystems and the available memory, and compares the
baseline; it returns non-zero when the baseline changed, is missing or is
empty. A changed start time or restart count of a production container
means: stop, and find out why before any further step.

Now copy the artifacts from the build machine into the new transfer
directory. No `--delete`, no other target:

```sh
# On the BUILD machine. HOST is the host's SSH name.
scp -- "$out/komika-postgres-image.tar.gz" "$out/komika-postgres-kit.tar.gz" "$out/image-id.txt" "$out/SHA256SUMS" HOST:/srv/komika-pg-phase5a-r5/transfer/
```

Write the env file at `$env_file` with an editor under `umask 077`.
The rehearsal needs an application dataset already in PostgreSQL 18 format,
including the `work`, `chapter` and `users` tables. Prepare a consistent
PostgreSQL dump and record its size, hash and expected row counts separately.
This kit does not load application data. After `r up`, restore that dump into
the rehearsal's empty `komika` database before taking its first backup. Keep
the dataset private and never connect a serving application to the rehearsal.

```sh
# C. Hashes, inputs and space first; only then the load. Nothing is built or pruned.
preflight_c() {
  test "$gate" = B || { echo "STOP: block B has not passed in this shell"; return 1; }
  (cd -- "$base/transfer" && sha256sum SHA256SUMS && sha256sum -c SHA256SUMS) || { echo "STOP: a transferred file does not have its recorded hash"; return 1; }
  stat -c '%u %a %s %n' -- "$env_file" || { echo "STOP: the env file is missing"; return 1; }
  test -f "$env_file" && test ! -L "$env_file" && test "$(stat -c '%u %a' -- "$env_file")" = "$(id -u) 600" || { echo "STOP: the env file is not your own regular file with mode 600"; return 1; }
  sed -n 's/^\([A-Z0-9_]*\)=.*/\1/p' -- "$env_file"
  gzip -l -- "$base/transfer/komika-postgres-image.tar.gz"
  df -B1 -- "$docker_root" "$base"
  image_bytes=$(gzip -l -- "$base/transfer/komika-postgres-image.tar.gz" | awk 'NR == 2 {print $2}')
  test "${image_bytes:-0}" -gt 0 || { echo "STOP: image size unknown; nothing was loaded"; return 1; }
  need=$(( image_bytes + 3 * 1024 * 1024 * 1024 ))
  avail=$(df -B1 --output=avail -- "$docker_root" | tail -n 1 | tr -d ' ')
  if test "$avail" -ge "$need"
  then echo "headroom ok: $avail free on Docker's data root >= $need (image + 3 GiB floor)"
  else echo "STOP: image and floor need $need bytes on Docker's data root, $avail are free; nothing was loaded"; return 1
  fi
  tar -xzf "$base/transfer/komika-postgres-kit.tar.gz" -C "$kit" --strip-components=1 --no-same-owner || { echo "STOP: the kit archive could not be unpacked"; return 1; }
  gzip -dc -- "$base/transfer/komika-postgres-image.tar.gz" | docker load || { echo "STOP: docker load failed"; return 1; }
  docker image inspect --format '{{.Id}}' "$image" || { echo "STOP: the loaded image does not carry the expected tag"; return 1; }
  cat -- "$base/transfer/image-id.txt"
  rm -- "$base/transfer/komika-postgres-image.tar.gz"
  after load || return 1
  echo "BLOCK C OK"
}
preflight_c && gate=C; echo "gate=$gate (C expected)"
```

Read as well, or stop: the first hash equals the one you wrote down on the
build machine; the `sed` line prints key NAMES only (never print the file)
and they are the keys listed above; the env file has a new `KOMIKA_R2_PREFIX` that starts
with `rehearsal/`, `KOMIKA_PRODUCTION_R2_PREFIX` set to production's real
prefix, `KOMIKA_PG_INIT_NEW` present and no `KOMIKA_R2_VERIFY_TLS`; the two
image IDs printed at the end are equal (if the two machines use different
image stores they can differ: then `preflight` must still report the seven
kit files as identical, and you record both IDs). The space check runs before
anything is loaded: it asks for the uncompressed image
and the 3 GiB floor on Docker's data root. On a host with several times that
free it passes; it stays in the block because the next host may be smaller.
The image's size comes from `gzip -l`; a missing or zero value stops the
block (`gzip -l` counts modulo 4 GiB, so for an image tarball of 4 GiB or
more read the size on the build machine instead; this image is far below
that). The `rm` line names one file that this run copied here; its copy
stays on the build machine. A pasted block whose last line prints no
`gate=` line at all has failed too (the shell gave the line up): read it as
a `STOP`.

If `STOP after load` is the only `STOP` of block C, the image is loaded and
the transferred tarball is removed already: everything of block C has
happened, and only the comparison of the other containers failed. Read the
diff that was printed. If production is untouched (for example a container
of another project was restarted by its owner during the load), set the
gate by hand with `gate=C` and go on to block D; if a production container
changed, stop and find out why. Never paste block C a second time: its
first check would fail, because the tarball is gone.

```sh
# D. One operator at a time, the account's counters, then the selection.
r() { ./rehearsal.sh "$@"; }
preflight_d() {
  test "$gate" = C || { echo "STOP: block C has not passed in this shell"; return 1; }
  test ! -L /run/lock/komika-pg-rehearsal-operator.lock && exec 9>> /run/lock/komika-pg-rehearsal-operator.lock \
    || { echo "STOP: the operator lease file is a symbolic link or could not be opened"; return 1; }
  flock -n 9 || { echo "STOP: another operator session holds the lease"; return 1; }
  echo "operator lease held"
  date -u +%Y-%m-%dT%H:%M:%SZ | tee "$records/r2-metrics-before-time.txt"
  cd -- "$kit" || return 1
  export KOMIKA_REHEARSAL_PROJECT=komika-pg-rehearsal-$run
  export KOMIKA_COMPOSE_ENV_FILE=$env_file
  export KOMIKA_REHEARSAL_ROOT=$root
  env | cut -d= -f1 | grep -E '^(KOMIKA|DOCKER|COMPOSE|PGBACKREST)_' | sort
  test "$(env | cut -d= -f1 | grep -E '^(KOMIKA|DOCKER|COMPOSE|PGBACKREST)_' | sort | tr '\n' ' ')" = "KOMIKA_COMPOSE_ENV_FILE KOMIKA_REHEARSAL_PROJECT KOMIKA_REHEARSAL_ROOT " \
    || { echo "STOP: the shell exports another KOMIKA_, DOCKER_, COMPOSE_ or PGBACKREST_ variable"; return 1; }
  echo "BLOCK D OK"
}
preflight_d && gate=D; echo "gate=$gate (D expected)"
```

The operator lease is your own file and serializes operators for the whole
drill. It is opened for appending and only when it is not a symbolic link,
so the block never truncates a file that somebody else put behind that name.
It is deliberately not the kit's lock file
(`/run/lock/komika-pg-rehearsal.lock`): never open or `flock` that one
yourself, or every kit command is refused. Before the first repository
operation, read the account's R2 numbers and write them down with the time
just recorded: stored bytes and object count of the bucket, and the Class A
and Class B operation counts of the current billing month (Cloudflare
dashboard: R2, the bucket's metrics, and the account's R2 usage). Note that
these counters lag; write down how long you waited. The kit has no command
for account metrics and none that lists production's prefix.

### While `restore` runs

`restore` can take a long time and load the disk; nothing in the kit limits
block I/O. Run it at a quiet time for production. Open a second window in the
same tmux session (`Ctrl-b c`), paste this block once, and start the loop
(`watch_step restore`). It is read-only, takes no
lock and writes only into the run's `records` directory; stop it with Ctrl-C
when the step has ended.

```sh
# W. Second window, read-only watch. Paste once after block D has passed in the first window.
run=phase5a-r5
base=/srv/komika-pg-$run
records=$base/records
docker_root=$(docker info --format '{{.DockerRootDir}}')
cd -- "$base/kit" && export KOMIKA_REHEARSAL_PROJECT=komika-pg-rehearsal-$run KOMIKA_COMPOSE_ENV_FILE=$base/private/rehearsal.env KOMIKA_REHEARSAL_ROOT=$base/root
r() { ./rehearsal.sh "$@"; }
others() {
  docker ps -aq --no-trunc | xargs -r docker inspect --format '{{.Id}} {{.Name}} {{.State.StartedAt}} {{.RestartCount}} {{.State.Status}} {{with index .Config.Labels "cc.komiq.rehearsal"}}{{.}}{{end}}' | awk 'NF == 5' | sort
}
watch_step() {
  while :; do
    date -u +%Y-%m-%dT%H:%M:%SZ
    printf 'free_bytes_docker_root=%s\n' "$(df -B1 --output=avail -- "$docker_root" | tail -n 1 | tr -d ' ')"
    printf 'free_bytes_base=%s\n' "$(df -B1 --output=avail -- "$base" | tail -n 1 | tr -d ' ')"
    awk '/MemAvailable:/{print}' /proc/meminfo
    vmstat 5 2 | awk 'END {print "iowait_percent=" $16}'
    r archive-status 2> /dev/null | grep -E '^(archived_count=|ready_segments=|pg_wal_bytes=)' || echo "archive-status: no running source (expected during restore)"
    if test -s "$records/others-before.txt" && others | cmp -s -- "$records/others-before.txt" -
    then echo "baseline: unchanged"
    else echo "STOP: a container outside the rehearsal changed, or the baseline is missing or empty"
    fi
    sleep 25
  done 2>&1 | tee -a "$records/watch-$1.txt"
}
```

Each round takes about half a minute and prints: the time, the free bytes on
Docker's data root and on the run's directory, the available memory, the
I/O wait of the last five seconds (`vmstat`, column `wa`), PostgreSQL's
archiver counters with the number of WAL segments that wait and the size of
`pg_wal`, and whether the baseline of the other containers still holds.
During `restore` there is no source container, so the archiver line is
replaced by a note.

Abort when any one of these holds; agree on them before the step starts:

- free bytes on Docker's data root below 20 GiB, or fallen by more than
  12 GB since the start of the step;
- `pg_wal_bytes` above 6 GB;
- `MemAvailable` below 4 GiB;
- a production container's restart count or start time changed (`after`, or
  the `STOP` line of the watch), or production answers slowly;
- I/O wait above about 30 percent for more than a minute (the kit has no I/O
  limit; this is the only brake);
- any `r` command prints `REFUSED`.

The abort action, in this order:

1. In the window of the step press Ctrl-C once. Or, from the second window,
   send SIGTERM to the recorded process. `r status` prints one line
   `operation recorded: subcommand=... pid=N since=... process=...`. Send
   SIGTERM (`kill -TERM N`) only if that line ends in `process=running` for
   that pid and `ps -o args= -p N` shows `rehearsal`. With `process=NOT
   running (it ended without cleanup; the lock itself is free)` the step has
   already ended and the number may belong to another process by now: send
   nothing and go on to steps 2 and 3. With `process=running as another
   user` or `operation recorded: none` send nothing either. The kit unwinds
   and removes its job container. Do not press Ctrl-C a second time, never
   use `kill -9` (the job container would keep running without its client),
   and never `docker rm` anything by hand.
2. `r status`. It must list no job container. If it shows one or an
   `UNRESOLVED` line, the next command resolves it.
3. `r stop-source` (it gives PostgreSQL up to 240 seconds to stop).
4. `r destroy-source-volumes --yes-delete-rehearsal-volumes` (this returns
   the disk space).
5. Only if `stanza-create` has already run: `r repo-destroy
   --yes-delete-rehearsal-repo`. Before `stanza-create` the rehearsal prefix
   holds nothing.
6. `r down`, then `r status` (everything absent), then `after abort`.
7. The removal of the run's directory with its identity check, as at the end
   of "Full drill order".

A `restore` that is aborted leaves its target directory; remove it with the
identity check of "Full drill order" before step 7.

### Full drill order

In the first window, after block D printed `gate=D`, one line at a time.
Never paste this block whole: unlike blocks A to D it does not stop itself,
so the lines after a failed step, among them the removal of the source
volumes and of the rehearsal repository, would still run.
Every line prints the step's exit status and then runs `after`; stop at the
first status that is not 0 and at the first `STOP`. The long steps carry
`time`, so their wall time is on record. Start `watch_step restore` in the
second window before the `restore` line. Never run `r drill` on a host that keeps the source and has
little disk: this order removes the source before it restores.

```sh
test "$gate" = D && echo "pre-flight passed: the drill may start" || echo "STOP: the pre-flight has not passed in this shell"
r preflight --init-root ; echo "exit=$?" ; after preflight
r up ; echo "exit=$?" ; after up
r psql-check ; echo "exit=$?"
# Stop here until the PostgreSQL application dataset is loaded and its row counts checked.
r stanza-create ; echo "exit=$?"
time r backup full --first ; echo "exit=$?" ; after backup-full
r logs postgres --tail 99999 2>&1 | grep -ci 'dropped' | tee "$records/dropped-wal-lines.txt"
r info --json --out "$root/full-info.json" ; echo "exit=$?"
r repo-usage --out "$root/usage-after-full.json" ; echo "exit=$?"
r check ; echo "exit=$?"
marker=$(r sql-marker --label before-pitr) ; echo "exit=$?" ; printf '%s\n' "$marker"
T=$(printf '%s\n' "$marker" | sed -n 's/^database_time=\([^ ]*\).*/\1/p')
test -n "$T" && printf '%s\n' "$T" > "$records/target-time.txt" && echo "T=$T"
r record-settings --out "$root/source-settings.json" ; echo "exit=$?"
r sql-marker --label after-pitr ; echo "exit=$?"
time r backup incr ; echo "exit=$?" ; after backup-incr
r info --json --out "$root/incr-info.json" ; echo "exit=$?"
r repo-usage --out "$root/usage-after-incr.json" ; echo "exit=$?"
r archive-status ; echo "exit=$?"
r status ; echo "exit=$?"
r stop-source ; echo "exit=$?" ; after stop-source
r destroy-source-volumes --yes-delete-rehearsal-volumes ; echo "exit=$?" ; after destroy-source-volumes
time r restore --target "$root/pitr" --target-time "$T" --source-settings "$root/source-settings.json" --keep --json-out "$root/pitr.json" --expect-marker before-pitr --expect-no-marker after-pitr ; echo "exit=$?" ; after restore
r restore-cleanup --target "$root/pitr" ; echo "exit=$?" ; after restore-cleanup
r repo-usage --out "$root/usage-before-destroy.json" ; echo "exit=$?"
r repo-destroy --yes-delete-rehearsal-repo ; echo "exit=$?" ; after repo-destroy
r down ; echo "exit=$?" ; after down
r status ; echo "exit=$?"
```

What to check after each step:

| Step | Check |
| --- | --- |
| `preflight --init-root` | Last line `PREFLIGHT OK`; `new_project=` line (nothing of this project existed); seven image files identical; both prefixes as intended; `source_storage=` line; both subnets free; the root's owner, mode and mount. |
| `up` | `postgres state=running health=healthy`; a new cluster was initialized (first `up` on a new prefix). `up` itself has compared the created container's real limits, environment and mount sources with the model. Then set `KOMIKA_PG_INIT_NEW=0` in the env file with an editor. |
| `psql-check` | `application_health=ok`; PostgreSQL 18.6; data checksums on; SCRAM; the role is no superuser; locale and `pg_trgm` as expected. |
| `stanza-create` | Exit 0. |
| `backup full --first` | Exit 0; the wall time. |
| the `grep -ci 'dropped'` line | A number, kept in `records/dropped-wal-lines.txt`. It counts lines of the PostgreSQL log that mention dropped WAL. With `archive-push-queue-max` (1 GiB) pgBackRest may drop queued segments once the stanza exists when uploads fall behind. That is harmless for the drill (the base of the point-in-time restore is the full backup taken afterwards) and explains an `archive_bytes` below the estimate; record the number. |
| `info --json --out`, `repo-usage --out` | Both files exist under the root: per-backup database and repository sizes, and the object count and bytes under the rehearsal prefix. |
| `check` | Exit 0. |
| `sql-marker --label before-pitr` | `T=` printed, a UTC time; it is also in `records/target-time.txt`. If the shell is lost, read it from there. |
| `record-settings` | The file exists; it must be written before the source volumes are destroyed. |
| `sql-marker --label after-pitr` | Exit 0: a commit after T exists. |
| `backup incr` | Exit 0; the wall time. |
| second `info` and `repo-usage` | The incremental backup is listed; WAL bytes (`archive_bytes`) and backup bytes are separate numbers. |
| `archive-status` | `ready_segments=0`; `last_archived_time` later than `last_failed_time`; `pg_wal_bytes` stable. |
| `status` | Free bytes above the floor on both filesystems; no job container, no `UNRESOLVED` line, no kept restore. |
| `stop-source` | Only the rehearsal's containers were removed; `after` says the other containers are unchanged. |
| `destroy-source-volumes` | Both volumes removed; `after` shows the space is back; the rehearsal network is still there. |
| `restore` | Exit 0; the wall time; `marker expectations met: present=before-pitr absent=after-pitr`; `operator_markers=before-pitr`; `pitr.json` holds `restore_seconds`, `recovery_seconds`, `restored_bytes`, the per-backup sizes and `repository_usage`. |
| `restore-cleanup` | `removed_containers=1 removed_networks=1`; the restored directory stays. |
| `repo-destroy` | Exit 0; `repository_files_after=0`. |
| `down`, `status` | Services, volumes, network, jobs and kept restores of the project are all absent. |

After an interruption (Ctrl-C, a lost session, a refused step), run `r status`
first. If it shows a job container or an `UNRESOLVED` line, run `r
stop-source` (it waits for a late creation, at most 30 seconds, removes the
container and clears the record), then `r up` again and continue; see "Notes
on single steps". To give the run up instead, follow the abort action under
"While `restore` runs".

When the results are accepted, remove the restored directory. It belongs to
the image's postgres user (uid 999), so this needs root. Stop the watch loop
first. Then run the same `sudo` line alone, as before block B, so that no password
prompt can swallow a pasted line; the chain itself stops at `sudo -n true`
when a prompt would come. The identity check and the removal are one command: the `rm` is the
last link of the `&&` chain, so it cannot run unless every test before it
passed and `IDENTITY OK` was printed for exactly the path it names. The first
test ties the variable to the literal path of the `rm` (edit both when your
`base` differs).

```sh
sudo -n true || sudo -v
```

```sh
target="$root/pitr"
test "$target" = /srv/komika-pg-phase5a-r5/root/pitr \
  && test "$target" = "${KOMIKA_REHEARSAL_ROOT:?}/pitr" \
  && test "$(realpath -e -- "$target")" = "$target" \
  && test -d "$target" && test ! -L "$target" \
  && test -f "$root/.komika-rehearsal-root" \
  && test "$(stat -c %d -- "$target")" = "$(stat -c %d -- "$root")" \
  && mounts=$(findmnt -rn -o TARGET) && test -n "$mounts" \
  && test -z "$(printf '%s\n' "$mounts" | grep -F -- "$target")" \
  && sudo -n true \
  && test "$(sudo cat -- "$target/PG_VERSION")" = 18 \
  && running=$(docker ps -aq --filter "label=cc.komiq.rehearsal.restore-target=$target") \
  && test -z "$running" \
  && echo "IDENTITY OK: $target" \
  && sudo rm -rf --one-file-system -- /srv/komika-pg-phase5a-r5/root/pitr \
  && echo "REMOVED: /srv/komika-pg-phase5a-r5/root/pitr"
after remove-pitr
```

No `REMOVED` line means nothing was removed: find out which test failed
before anything else. The check says: the directory is the `pitr` child of
this rehearsal's root and the literal path of the `rm`, it is not a link and
not reached through one, the root's marker is beside it, it is on the root's
own filesystem with nothing mounted at or below it, it holds a PostgreSQL 18
data directory, and no restore container still uses it. The mount list and
the container list are read into variables first, and the chain goes on
only when `findmnt` and `docker ps` themselves succeeded and the mount list
is not empty: a command that fails prints nothing, and nothing must never
count as "nothing is mounted" or "no container".

Finally read the account's R2 numbers again, after the counters' lag, and
record them beside the first reading and the three `usage-*.json` files. The
reports under `$root`, the records, the PostgreSQL dump and the env file stay
until the results are accepted.

Before the last removal, copy the evidence off the host: the removal deletes
the records (`watch-*.txt`, `others-*.txt`, the target
time) and every report under `$root`. Run this on the build machine, into
the `out` directory of the build; it copies the records and the top-level
JSON reports only, never the `private` or the `input` directory:

```sh
# On the BUILD machine. HOST is the host's SSH name. Nothing is deleted on either side.
mkdir -m 0700 -- "$out/evidence"
scp -r -- HOST:/srv/komika-pg-phase5a-r5/records "$out/evidence/"
scp -- 'HOST:/srv/komika-pg-phase5a-r5/root/*.json' "$out/evidence/"
ls -l -- "$out/evidence" "$out/evidence/records"
```

Read the listing: the reports (`pitr.json`, `source-settings.json`, the two `*-info.json` and the
three `usage-*.json`) and the records are there and not empty. Only then
the one directory of this run, which holds a copy of production's data and
the env file, is removed the same way as the restored directory, again as
one command. Close the second window first (its working directory is inside
the run's directory). The removal needs `sudo`: the run's files belong to
you, but the directory itself is an entry of `/srv`, which belongs to root, so
a plain `rm` empties the directory and then fails on the directory itself
(seen on the test host on 2026-10-12). Run the `sudo` line alone once more
before the block if a long time has passed; the chain stops at `sudo -n true`
when a password would be needed.

```sh
cd / \
  && test "$base" = /srv/komika-pg-phase5a-r5 \
  && test "$(realpath -e -- "$base")" = "$base" && test ! -L "$base" \
  && test ! -e "$base/root/pitr" \
  && test "${KOMIKA_REHEARSAL_PROJECT:?}" = komika-pg-rehearsal-phase5a-r5 \
  && running=$(docker ps -aq --filter "label=cc.komiq.rehearsal=${KOMIKA_REHEARSAL_PROJECT:?}") && test -z "$running" \
  && mounts=$(findmnt -rn -o TARGET) && test -n "$mounts" \
  && test -z "$(printf '%s\n' "$mounts" | grep -F -- "$base")" \
  && sudo -n true \
  && echo "IDENTITY OK: $base" \
  && sudo rm -rf --one-file-system -- /srv/komika-pg-phase5a-r5 \
  && echo "REMOVED: /srv/komika-pg-phase5a-r5"
```

The loaded image, the lock file `/run/lock/komika-pg-rehearsal.lock` and the
operator lease file may stay. Nothing is pruned, and no image, volume or
network is removed by hand (the one exception is the image tag of your own
earlier run before a new run, see "Pre-flight on the host").

The point in time. `--target-time` is the `database_time` that
`sql-marker --label before-pitr` printed (UTC, `Z` or `+00:00`). The second
marker is part of the order for two reasons. First, the restore report and the
output must show `operator_markers=before-pitr` only; `--expect-marker LABEL`
and `--expect-no-marker LABEL` (both repeatable) make the kit itself fail the
restore when a marker that must be there is missing or one that must not be
there is present, so a restore to the wrong time cannot pass unnoticed.
Second, PostgreSQL reaches a target time only when a later commit exists in
the archived WAL: without one it ends recovery with `recovery ended before
configured recovery target was reached` and shuts down. The kit then fails at
once with that line and the advice to choose an earlier time.

Other subcommands: `r status` (containers, health, volumes, job containers,
kept restores, the operation recorded in the lock file, unresolved job
containers, `watch-free`, backup status, `pgbackrest info`), `r info [--json
[--out FILE]]` and `r repo-usage [--out FILE]` (both also work after the
source is gone, as long as the rehearsal network exists; see "The budget
record" below), `r logs [postgres|postgres-backup] [--tail N]`,
`r up --with-scheduler` (also starts the backup scheduler, after the first full
backup), `r drill --target NEWDIR --json-out FILE` (the two-marker PITR drill
below; needs the running source and therefore the disk space for source and
target at once).

Notes on single steps:

- Job containers are named `<project>-job-<8 hex>`, metadata readers
  `<project>-meta-<8 hex>`, and both carry the rehearsal label and explicit
  log bounds (`json-file`, `max-size=10m`, `max-file=3`), like the restore
  container. The kit removes its own container when the step ends, also
  after Ctrl-C, SIGTERM or a lost terminal (SIGHUP; under `nohup` the step
  keeps running). One case cannot be closed in the same run: the client was
  killed while its create request was still on its way, and the container
  did not appear within two seconds. The kit does not call that "absent". It
  prints `UNRESOLVED` with the container's name and records the name in the
  lock file. From then on `status` reports it, every other subcommand that
  takes the lock refuses and names it, and `stop-source` or `down` waits
  until the interruption is at least 30 seconds old (never longer than 30
  seconds, whatever time the record holds), removes the container if it has
  appeared and clears the record. The daemon may still create a
  container later than that; the next step would then find it by its label
  and refuse again, and `stop-source` removes it. After an
  interruption, run `r status` before continuing. After a kill that no handler
  can catch (SIGKILL, power loss), `stop-source` or `down` removes leftovers.
- `record-settings` must run after the last change of a source ceiling and
  before `destroy-source-volumes`.
- `archive-status` prints PostgreSQL's own counters since the cluster was
  created. Archive attempts before `stanza-create` fail and stay in
  `failed_count`. What matters after `backup`:
  `last_archived_time` is later than `last_failed_time`, `ready_segments=0`,
  and `pg_wal_bytes` no longer grows.
- `drill` and `restore` print the row counts of `work`, `chapter` and `users`
  of the restored `komika` database. A backup taken before loading the
  application dataset has no such tables; the step then fails and says so.
- `up` in a rehearsal starts an existing cluster or initializes a new one on
  an empty repository. It never restores. When the source volumes are empty or
  absent and the rehearsal prefix already holds a backup, `up` is refused
  before Compose starts whenever the host can read the repository (the data
  volume is absent and the rehearsal network exists), and in every other case
  by the image itself before PostgreSQL starts; both refusals name `restore
  --target NEWDIR --json-out FILE`. A new drill uses a new prefix, whose
  repository is empty.
- `restore` and `drill` need a Linux Docker host. `restore` works after the
  source containers and volumes are gone, from the local image, the env file,
  the settings record and R2. Without `--keep` it stops and removes its own
  container and network; the restored directory always stays.
- A `restore` that fails after it began leaves its target directory behind,
  owned by the image's postgres user (uid 999), so the caller cannot remove
  it. The kit prints the exact directory and the command
  (`sudo rm -rf -- DIR`). Remove it or choose a new `--target` name; a target
  must not exist. The same holds for a successful restore once its results are
  accepted.
- `repo-destroy` runs `pgbackrest stop` and `pgbackrest --force stanza-delete`
  in a mount-free throwaway container against the env file's rehearsal prefix
  and then prints the number of files left. The image checks project and
  prefix again before each pgBackRest command. It needs the rehearsal network,
  so it comes before `down`.
- `down` and `stop-source` never remove volumes; only
  `destroy-source-volumes` does, and only the two labelled volumes. Both
  remove the service containers and every job and metadata container of this
  project. An object they may not touch (a container or network with one of
  the project's names but without its label, a container with the label but
  none of the name patterns, a kept restore, a network that a foreign
  container still uses) no longer stops the rest: each one is reported on a
  `REFUSED:` line with the reason, everything else is removed, and the exit
  status is 1.
- In a rehearsal the image accepts only the pgBackRest command lines this kit
  sends (an allowlist of commands, options and parameter shapes in
  `safety.py`). Everything else is refused before pgBackRest runs, including
  `--reset-*`, `--no-*`, `--config*`, `--repo*`, `--stanza`, `expire` and
  `repo-rm`.
- Network. The two services and every repository reader join only the rehearsal bridge `172.30.21.0/24`; the image identity
  check runs with `--network none`; a restore has its own bridge
  `172.29.42.0/24`. No rehearsal container joins Docker's default bridge.
  Both bridges have outbound access, because PostgreSQL and pgBackRest must
  reach R2, and that outbound traffic is not filtered: a container on them can
  reach whatever the host's forwarding rules allow.

`KOMIKA_REHEARSAL_NO_REPO=1` in the env file selects a local proof without any
repository: every `KOMIKA_R2_*` value and `KOMIKA_PRODUCTION_R2_PREFIX` must
then be absent (the wrapper and the image both refuse them), pgBackRest is
never configured, PostgreSQL runs with `archive_mode=off` and logs a loud
warning, and `stanza-create`, `backup`, `check`, `info`, `drill`, `restore`,
`repo-destroy` and `up --with-scheduler` are refused. It proves the startup
and storage guards; it proves nothing about backup or recovery.

For another rehearsal use a new project, prefix and root: `preflight
--init-root` refuses to create a root while any container, volume or network
of the project's names or with its label exists. Finish `restore-cleanup`
before the next restore: the restore bridge `172.29.42.0/24` is fixed. Keep
the restored directory and reports until the independent results are
accepted. Never prune shared Docker resources.

### The budget record

Three things keep the storage and request numbers of a drill:

- `r info --json --out FILE` writes pgBackRest's own `info` JSON to a new
  file under the root (the same allowlisted, read-only command as `r info
  --json`). Per backup it holds the database size (`info.size`), what the
  backup added (`info.delta`) and the repository sizes
  (`info.repository.size` and `.delta`). Store it after the full and after
  the incremental backup.
- `r repo-usage [--out FILE]` lists the rehearsal prefix (`repo-ls`, read-only)
  and prints the object count and the bytes in total, under `archive/` (WAL)
  and under `backup/`, the number of WAL segment files, the number of objects
  larger than one upload part, and `write_requests_floor`. It takes no prefix,
  path or repository argument: the prefix is the env file's, which must be a
  `rehearsal/` prefix, and the image checks it again.
- The JSON of `restore` and `drill` keeps `backups` (label, type, times, WAL
  range and the four sizes of every backup) and `repository_usage` (the same
  numbers as `repo-usage`, taken just before the restore).

The WAL archived in an interval is the difference of `archive_bytes` between
two `repo-usage` files. An object count is not a request count.
`write_requests_floor` is the least number of Class A writes that the objects
present now have cost: one for an object of up to one part, else one per part
plus two (pgBackRest's S3 part size is its default, 5 MiB; the kit does not
set it). On top come objects that were rewritten or removed (`backup.info`,
`archive.info` and their copies on every backup), the list requests (every
`info`, `check`, `backup`, `repo-usage`, WAL push and start of the image
lists objects), and retries. Only the account's own metrics show those; read
them before and after the drill, as the pre-flight says.

### What the tooling cannot check

You must make sure of these yourself:

- The production prefix you entered is the real one. A wrong value weakens
  one comparison (the rehearsal prefix against production's). It does not
  remove the other fences: the rehearsal prefix must still start with
  `rehearsal/`, and the image still refuses every pgBackRest command line
  outside its allowlist.
- Which key the env file holds. A token that can only write below
  `rehearsal/` would bound a mistake at the provider; a drill that uses
  production's own key has no such bound, and then the prefix rules and the
  allowlist of this kit are the only fences. Say in the drill record which
  kind of key was used.
- The image was built from the reviewed tree. The kit proves that image and
  kit directory carry the same seven files, nothing about the base image, the
  installed packages or the executables; compare the hash of the image file.
- `docker` on `PATH` is the real client; no production resource carries a `cc.komiq.rehearsal` label.
- Memory, CPU, PID and log limits are set and compared; block I/O is not
  limited: Compose's `blkio_config.weight` is accepted by Docker but has no
  effect unless the host uses the BFQ scheduler or enables `io.cost`, and a
  throughput limit needs the host's own device path, which the fixed topology
  cannot name. Run the restore at a quiet time and watch the
  host and production's response times.
- The disk gate is an estimate before a step, not a reservation (see "Disk
  space"). The operation lock serializes this kit, not the host.
- R2 request counts and the account's totals: only the provider's metrics
  have them.

### Local drill against a throwaway S3 store

The repository half can be proven without R2 on a development Linux host
(for example the VM of a local Docker installation) against an S3 server that
you start yourself with a self-signed certificate. One env file key exists for
this and for nothing else: `KOMIKA_R2_VERIFY_TLS=0`. It is accepted only when
all three hold: the selection is a rehearsal, the project name ends with
`-local`, and `KOMIKA_R2_ENDPOINT` is a literal private IPv4 address (10/8,
172.16/12 or 192.168/16, no host name, no port). Anything else is refused
before any Docker call, the image checks the same three conditions again, and
the hosted path never reads the key. It becomes pgBackRest
`repo1-storage-verify-tls=n`.

The endpoint must be reachable from the rehearsal bridge and from the restore
bridge without any extra network on the rehearsal containers. Publish the S3
server's TLS port on port 443 of a private address of the host itself, for
example the gateway address of the store's own Docker network
(`docker run -p 10.213.4.1:443:9000 ...` on a network created with
`--subnet 10.213.4.0/24 --gateway 10.213.4.1`), and use that address as the
endpoint. The record `docs/audits/postgres-phase5a-hosted.md` has the complete
local run.

The runbook itself can be rehearsed there too: the pre-flight blocks A to D,
the watch block, the drill lines and the two removal blocks run unchanged on
a Linux test host against such a store. Do that once before the first run
on a host that carries production. What differs from production, and
nothing else:

- The run name ends in `-local` (for example `run=b7-local`: project
  `komika-pg-rehearsal-b7-local`, directory `/srv/komika-pg-b7-local`).
  Replace the run name everywhere, also in the literal paths of the two
  removal blocks and of the evidence copy.
- The env file names the store: `KOMIKA_R2_ENDPOINT` is the literal private
  IPv4 address and `KOMIKA_R2_VERIFY_TLS=0` is present, so block C prints
  that key name as well.
- The store's container runs before block A and stays untouched to the end.
  It is part of the baseline of the other containers, and its files live
  outside the run's directory.
- When the test host is also the build machine, the image tag exists there
  and block A stops on it, rightly. Save the image, remove the tag of your
  own build, and let block C load the tarball; copy the artifacts and the
  evidence with `cp` in place of `scp`.
- There are no account metrics to read.

A host where a container of another project bind-mounts `/`, `/srv`,
Docker's data root, a parent of it or its `volumes` directory (monitoring
agents often mount `/`) is stopped by block A. That is no weakness of the
block: the kit refuses the same host, at `preflight` when the mount overlaps
the rehearsal root and at `up` when it overlaps the volumes. Containers of
other projects as such stop nothing. When one of them is created, restarted
or removed during the run, `after` reports it with the diff; read the diff
before going on.

## Hosted commands: separate recovery path (Phase 6+)

The sections about hosted installation/replacement require separate operator
authorization. Use a fresh shell with NO `KOMIKA_REHEARSAL_*` variables.
Helpers refuse hosted marker drills/restores unless `--hosted-recovery` is
explicitly supplied; rehearsal variables with the hosted file always fail.
These commands belong to the hosted checkout, never to a rehearsal copy of
this directory. Name the checkout's deployment directory once with an absolute
path, select the hosted project explicitly, then use `hc` below:

```sh
hosted_deploy=/absolute/hosted-checkout/deploy   # holds docker-compose.yml, .env and postgres/
export KOMIKA_COMPOSE_FILE="$hosted_deploy/docker-compose.yml"
export KOMIKA_COMPOSE_ENV_FILE="$hosted_deploy/.env"
export KOMIKA_COMPOSE_PROJECT=komika
hc() { docker compose --env-file "$KOMIKA_COMPOSE_ENV_FILE" -f "$KOMIKA_COMPOSE_FILE" -p "$KOMIKA_COMPOSE_PROJECT" "$@"; }
```

`KOMIKA_PG_ALLOWED_CIDRS` is a comma-separated list of strict CIDRs; default
`172.21.0.0/16,fd00:21::/64`. Only RFC1918 IPv4 and IPv6 ULA subnets are accepted.
Public ranges, loopback/link-local, default routes, host bits and malformed/empty
items fail startup. Match it to the selected Compose bridge. Application access
is TCP SCRAM only; the Unix socket permits only postgres OS-user peer access.
Health uses TCP SCRAM and stays false until recovery ends.

## First hosted installation

Set `KOMIKA_POSTGRES_PASSWORD` to random hex (at least 24 URI-safe characters).
Set all five `KOMIKA_R2_*` variables in the mode-0600 `.env` beside the hosted
Compose file. Endpoint is an
HTTPS hostname; prefix has no leading slash and must be dedicated to this stack.
Use a throwaway prefix for rehearsal. Set `KOMIKA_PG_INIT_NEW=1` only for a
reachable, empty prefix. Existing stanza metadata without a full backup refuses
empty-volume startup; finish provisioning on the retained original volume.

```sh
hc up -d --build postgres
hc exec -T --user postgres postgres /opt/komika-postgres/health.sh
hc exec -T --user postgres postgres /opt/komika-postgres/pgbackrest.sh stanza-create
hc up -d --build server
hc logs server
hc exec -T --user postgres postgres /opt/komika-postgres/backup.sh bootstrap-full
hc exec -T --user postgres postgres /opt/komika-postgres/backup.sh check
hc exec -T --user postgres postgres /opt/komika-postgres/backup.sh incr
hc up -d postgres-backup
```

Return `KOMIKA_PG_INIT_NEW=0` in `.env` after provisioning. Check both database and
backup health before public traffic.
No backup variables means local initdb without archiving and a loud warning.
Partial configuration, authentication/network/corruption errors and failed
restores fail startup. Do not bypass a failure with the new-install flag.

The scheduler runs full Sunday 02:00 UTC, check daily 02:00 UTC and incremental
daily 03:00 UTC, including Sunday. Two full chains are retained, normally about
7–14 days of recovery history. No R2 lifecycle rule may delete chain fragments.
It catches up one latest slot per job type, recomputing due work after every
job and draining slots crossed by a long-running backup before sleeping.
Attempts are persisted before work:
interrupted/failed slots are not repeated automatically after restart. Use an
explicit serialized retry (`backup.sh full`, `backup.sh incr`, `backup.sh check`)
to repair an error immediately. Manual jobs share the scheduler's kernel lock;
if busy, wait for the active job and retry. Atomic status lives in
`/var/lib/postgresql/komika-backup/status.json` (no credentials).

## Restore latest into a NEW empty named volume

Stop writers and backups first. This is a recovery replacement, not an online
inspection of the live volume. Pick unused names; `docker volume create` is not
proof of emptiness, so the wrapper refuses nonempty volumes without PG_VERSION.
Never point this override at an old volume. Keep the override for all subsequent
Compose operations until the recovered volume becomes the permanent config.

```sh
hc stop server postgres-backup postgres
docker volume create komika-pg-recovered-20261011
docker volume create komika-pg-recovered-socket-20261011
recovery_override=$(mktemp)
cat > "$recovery_override" <<'YAML'
volumes:
  postgres-data:
    external: true
    name: komika-pg-recovered-20261011
  postgres-socket:
    external: true
    name: komika-pg-recovered-socket-20261011
YAML
hcr() { docker compose --env-file "$KOMIKA_COMPOSE_ENV_FILE" -f "$KOMIKA_COMPOSE_FILE" -f "$recovery_override" -p "$KOMIKA_COMPOSE_PROJECT" "$@"; }
# Requires INIT_NEW=0. Empty PGDATA restores latest before PostgreSQL starts.
hcr up -d postgres
hcr logs postgres
hcr exec -T --user postgres postgres /opt/komika-postgres/health.sh
hcr exec -T --user postgres postgres \
  psql -X -w -U postgres -d komika -Atc "SELECT 'work=' || count(*) FROM work UNION ALL SELECT 'chapter=' || count(*) FROM chapter UNION ALL SELECT 'users=' || count(*) FROM users"
```

Compare counts with the recorded PostgreSQL snapshot.

```sh
hcr exec -T --user postgres postgres /opt/komika-postgres/backup.sh check
hcr exec -T --user postgres postgres /opt/komika-postgres/backup.sh full
# Only after row-count checks, fresh full and application smoke checks succeed:
hcr up -d server postgres-backup
```

Record backup label, restored bytes, timings and recovered horizon. Configure the
new volume names permanently before removing the temporary override. Old volumes
remain intact. Restored credentials must match `.env`; do not change the password
to make a failed restore appear to succeed.

## Point-in-time inspection in an empty directory

A rehearsal uses `r restore` and `r restore-cleanup` from
the rehearsal section. The hosted recovery form needs separate operator
authorization, the hosted exports above, no `KOMIKA_REHEARSAL_*` variable and
the explicit flag:

```sh
"$hosted_deploy/postgres/restore.sh" /absolute/new/pitr-directory --hosted-recovery \
  --source-settings /absolute/hosted-source-settings.json \
  --target-time '2026-10-11T12:34:56.123456Z' --keep \
  --json-out /absolute/pitr-report.json
```

Both forms run the same code. Restore-only mode reads the image and the
environment from the selected Compose file and env file, and works after the
source container and volume have been removed. It uses private tmpfs storage
for read-only metadata readers, without mounting or recreating the source
volume. The target must be a NEW directory. In a rehearsal it is a new name
directly under `KOMIKA_REHEARSAL_ROOT`. In hosted recovery its parent must
exist. Either way the helper queries DockerRootDir, inspects ALL containers and
volumes (including stopped containers and retained volumes), and refuses a
location that lies inside, above or at Docker's data, a volume mountpoint or a
bind-mount source, by path, by device and inode number, and on Linux by the
backing directory in the host mount table. This happens before mkdir, chown,
restore or markers. A remote Docker endpoint is refused. The helper creates a
distinct restore container with only the new bind directory and private tmpfs.
Its own bridge is `172.29.42.0/24`, with matching regenerated HBA; it never
joins the production or source network. No port is published and archiving is
off.

The five recovery ceilings are at least the hosted defaults and restored
`pg_controldata` values; a running marker source also supplies its current values.
Record settings AFTER all source ceiling changes and BEFORE deleting its volume
(rehearsal: `r record-settings --out FILE`):

```sh
"$hosted_deploy/postgres/pitr-drill.sh" record-settings --hosted-recovery --out /absolute/hosted-source-settings.json
# Reads SHOW for all five ceilings and captures LC_ALL=C pg_controldata.
```

`--source-settings` takes that validated JSON record. Each recovery ceiling is
the maximum of retained current/source control values, backup control values and
hosted defaults. Without a record the helper prints a warning; later WAL
ceiling increases may prevent recovery. Refresh the record after any increase.
They are max_connections (48), max_worker_processes (8), max_wal_senders (10),
max_prepared_transactions (0), max_locks_per_transaction (64). Never reduce these
for recovery. The helper waits up to 1,800 seconds for actual end of recovery
after `pg_ctl -w`.
Every promotion query has a subprocess timeout equal to the remaining deadline;
a late success is rejected.
`recovery_seconds` includes server startup and that poll, through promotion.

With `--keep` the helper prints `restored_directory`, `throwaway_container` and
`throwaway_network` and includes them in the JSON. The restored HBA permits
role `komika` over TCP only from `172.29.42.0/24`, plus the postgres peer
socket; source and production CIDRs are removed. Inspect the restored rows
and marker results before cleanup.

```sh
# Replace both names below with the exact printed names.
restore_container=komika-pitr-PRINTEDNAME
restore_network=komika-pitr-PRINTEDNAME-network
# Then stop and remove exactly these two, nothing else:
docker exec --user postgres "$restore_container" pg_ctl -D /restore -w -m fast stop
docker rm -v "$restore_container"
docker network rm "$restore_network"
```

Omit `--target-time` for latest inspection in an empty directory. Omit `--keep`
to stop/remove the container automatically and retain restored bytes. Helpers
leave the directory (including diagnostics) on success or failure. On a handled
error/Ctrl-C/SIGTERM they remove only their owned container/network. After
SIGKILL a rehearsal runs `r restore-cleanup --target DIR` (label and mount
checks precede stop and removal); hosted recovery inspects the exact
`komika-pitr-*` container and matching network and removes those two. Never
prune shared resources.

## Marker PITR drill

Run on the rehearsal stack with `r drill`, or as a separately authorized hosted
drill with `pitr-drill.sh --hosted-recovery`; both need an existing stanza and
backup and a running source. The rehearsal form needs the disk space for
source and restore target at once and refuses when that does not fit (see
"Disk space"); on a small host use the documented order with `r restore`
instead. The drill adds two uniquely named markers in its
own database `komika_pitr_drill`, created on first use; the application
database `komika` is not written. First commit →
database time T → WAL switch/archive acknowledgement and actual archive-get →
second commit → WAL switch/archive-get. It restores a pre-T backup to T into a
new directory and proves first present/second absent, with authenticated
application access and row counts. It always stops/removes its container, keeps
the directory for inspection, prints backup set/restore/recovery seconds/bytes,
and lists total prefix bytes from repo-ls. Request counts are printed if
pgBackRest reports statistics; otherwise it explicitly reports unavailable. R2 account metrics are
the billing authority; object counts are not request counts.

```sh
r drill --target "$KOMIKA_REHEARSAL_ROOT/drill-directory" --json-out "$KOMIKA_REHEARSAL_ROOT/drill-report.json"
# Hosted, separately authorized:
"$hosted_deploy/postgres/pitr-drill.sh" /absolute/new/drill-directory --hosted-recovery --json-out /absolute/drill-report.json
```

The final stdout line is one JSON object, also written verbatim to the NEW
`--json-out` path (outside the restored directory): `backup_label`, `backup_type`,
`restore_seconds`, `recovery_seconds`, `restored_bytes`, `repository_size_bytes`,
`wal_archive_min`, `wal_archive_max`, `restored_directory`,
`operator_markers` (the labels of `sql-marker` found in the restored server, in
commit order), `throwaway_container`, `throwaway_network` (names are null
without `--keep`). Repository size sums retained backup
`info.repository.delta` values from pgBackRest info; it excludes WAL/metadata.
Missing size/archive fields are null, never invented zeros. The separately
printed repo-ls byte sum includes WAL/metadata; request counts remain distinct.

Run twice in Phase 6. Marker checks prove the target boundary, not complete data
integrity; compare restored row counts and application behavior with recorded expectations.
Record actual WAL/day, incremental/full compressed sizes, peak R2 storage through
expiration of a third full, request counts and restore time at real size.

## Archive health and dropped WAL

```sh
hc ps postgres postgres-backup
hc exec -T --user postgres postgres cat /var/lib/postgresql/komika-backup/status.json
hc exec -T --user postgres postgres /opt/komika-postgres/pgbackrest.sh --output=json info
hc exec -T --user postgres postgres /opt/komika-postgres/backup.sh check
hc exec -T --user postgres postgres psql -X -w -U postgres -d komika -c \
  'SELECT archived_count, failed_count, last_archived_wal, last_archived_time, last_failed_wal, last_failed_time FROM pg_stat_archiver'
hc exec -T --user postgres postgres du -sh /var/lib/postgresql/18/docker/pg_wal
hc exec -T --user postgres postgres df -h /var/lib/postgresql
hc logs --since 24h postgres postgres-backup
```

In a rehearsal the same facts come from `r status`, `r archive-status`,
`r check`, `r info --json` and `r logs postgres --tail 500`.

Alert on incremental/check success age >26 h, full >8 days, job errors, growing
WAL, low host disk, archiver failures/lag and **every pgBackRest dropped-WAL
warning**. Alerting outside Docker health is a Phase 6 operational gate. A `.done`
WAL file alone is insufficient: pgBackRest may acknowledge dropped WAL.

`archive-push-queue-max=1GiB` deliberately preserves host availability at the
expense of PITR continuity during outages. It is checked only when pgBackRest
runs and is not a hard `pg_wal` cap. `max_wal_size` is not a cap either.
If WAL was dropped: stop application writes, repair credentials/network/storage,
run check, take an explicit new full, then run a new marker restore drill. Record
the gap. PITR cannot cross it; retain older chains but do not promise recovery
through missing WAL. Never delete live WAL manually. Monitor shared-host free
space separately; another project can consume headroom between checks.

R2 keys enter only postgres/backup container environments and are rendered into
postgres-owned 0600 files on container-private tmpfs. Host root, Docker admins,
and container root/postgres can read `.env`, Docker environment metadata, config
and process memory. Keys are absent from images/command lines and removed from
PostgreSQL's process environment. Rotate by editing mode-0600 `.env`, stopping
backup work, and recreating postgres and postgres-backup; inspect archive health
and run check afterwards. The restore helper passes keys through Docker stdin
rather than arguments or a host secret file; its temporary container has the
same residual Docker-admin exposure and is removed after use.

## Standby and promotion

An optional streaming standby with a fence and manual promotion is in
`STANDBY.md`. It does not replace anything in this file: replication is not
backup, and pgBackRest stays the only way back to an earlier time.

## Recovery health

Read-only checks for the hosted stack (use `hc` from "Hosted commands"). None of
them writes to PostgreSQL, the repository or a container:

```sh
hc exec -T --user postgres postgres-backup python3 /opt/komika-postgres/hosted.py status
hc exec -T --user postgres postgres-backup python3 /opt/komika-postgres/hosted.py status --local
hc exec -T --user postgres postgres-backup python3 /opt/komika-postgres/hosted.py reading
"$hosted_deploy/watch.sh" --url http://127.0.0.1:8080/health/ready --minutes 62
```

`status` prints one JSON object: job ages and errors from `status.json`; the
latest full and latest backup and the archive's first and last WAL from
`pgbackrest info`; `pg_stat_archiver` with ages, `failing` and `stalled`; `.ready` files (archive
backlog) and the age of the oldest; spool bytes; free bytes on the data volume;
recorded dropped-WAL events; `gap_open`; `recoverable_from`;
`archive_last_success` and `archive_last_wal`; and `health_problems`, the
reasons Docker health fails (empty when healthy). A value it cannot read is
`null` and its reason is in `unavailable`, never 0. `--local` skips the
repository request; the watch and Docker health use only local state.

`pgbackrest.sh` records each pgBackRest `dropped WAL file` warning with its
UTC time in microseconds in `/var/lib/postgresql/komika-backup/dropped-wal.log`
(the last 10,000 lines; one queue overflow writes one line per dropped segment).
A gap stays open until a full backup **starts** after the latest recorded drop.
pgBackRest's backup times have whole seconds, so a drop in the same second as a
full's start counts as still open. Every check uses pgBackRest's own start of
the full: `status` and the restore drill read it from `pgbackrest info`;
health, `status --local` and the watch use the start that `backup.sh full` or
the scheduler records from `pgbackrest info` after each full. A full taken any
other way closes the gap for `status` and the drill, but health stays red until
the next `backup.sh full` or scheduled full records one.

`recoverable_from` is the stop of the oldest backup, or after a drop the stop
of the first full backup that started after it. Point-in-time recovery is
possible only from there and never across missing WAL. `recoverable_until` is
always `null`: when the archiver last finished a segment (`archive_last_success`)
says nothing of the newest commit inside it, because old queued WAL can be
archived late. Only a restore shows how far the archived WAL reaches; the stop
of `latest_backup` is always reachable. A `.done` file, a passing `check` or a
healthy container is no proof of continuity; only a marker restore proves the
chain.

postgres-backup is unhealthy on the job-age rules above, on a job error, on an
open gap, when the archiver's last failure is newer than its last success and
there was no success for 15 minutes, when `.ready` files wait and neither the
last success nor the oldest waiting segment is newer than 15 minutes (a stalled
archiver without a recorded failure; an idle stack has no `.ready` file), and
when the data volume has less than 4 GiB free (the 1 GiB archive queue, the
2 GB `max_wal_size` and 1 GiB margin). Health makes no R2 request. `deploy.sh`
waits for it like every service; when it stays unhealthy, `deploy.sh` prints
`health_problems` before it stops (for example: run `backup.sh full`).

After a gap: repair the cause (credentials, network, storage), then

```sh
hc exec -T --user postgres postgres /opt/komika-postgres/backup.sh check
hc exec -T --user postgres postgres /opt/komika-postgres/backup.sh full
```

and run a marker restore drill (`pitr-drill.sh`). `gap_open` turns false once
that full has completed. Keep `dropped-wal.log`; it is the record of the gap.
`restore.sh --target-time` refuses a target inside a recorded gap (from the last
backup before the drop to the stop of the next full) when the source postgres
container runs. `restore.sh` without `--target-time` (the latest state) refuses
while a gap is open: recovery would stop at the first missing segment and
promote that older state without an error. In a real disaster the chain before
the drop may be all there is; then pass `--accept-gap`, and the restore goes
ahead with a warning. When the record cannot be read (the source postgres is not
running) both say so and go ahead; then read the record yourself before
trusting the result.

`reading` is the budget record for the dated duty (2026-10-20, 2026-10-27,
2026-11-03, 2026-11-10; act at 7 GB): raw WAL from `pg_stat_wal` since its
`stats_reset` and the total WAL position, and the repository's bytes split into
archived WAL and backups from one `repo-ls` listing (itself a few Class A list
operations). `prefix_over_action_threshold` compares only this pgBackRest prefix
with 7 GB; the threshold is account-wide (old Litestream data and other services
share the allowance), so read the account total and the Class A count from the
Cloudflare dashboard before deciding. Save each output with its date; nothing
runs on a schedule.

`watch.sh` is the read-only watch after a deploy. It runs until a wall-clock
deadline (`--minutes`, interval 30 s), not a number of rounds, and ends with
`covered_seconds` and every gap between rounds longer than twice the interval.
Each round shows the health answer, restarts and memory kills of the project's
containers, server log severities since the start (colour codes removed; JSON
`level` read too), PostgreSQL ERROR/FATAL/PANIC and dropped-WAL lines, and
`status --local`. A `null` gap or archiver state counts as unavailable, like no
answer; three such rounds in a row stop the watch. The round that starts at or
after the deadline is the last observation. A hard failure prints a `STOP` line
and the exit status is 1. It watches only an `http`/`https` URL without user
information or `%` in the host. It refuses komiq.cc, its subdomains and the
production address (any spelling), a `DOCKER_HOST` that is not a local `unix://`
socket and a Docker context other than `default`, unless `--production` is
given.

Role `komika` may execute `pg_ls_waldir()` and `pg_ls_archive_statusdir()` and
nothing more (not `pg_monitor`), so the server's `/metrics` WAL gauges work.
New clusters get it at initialization; the scheduler grants it on existing ones
at start and logs one line. Check it as the application role:

```sh
hc exec -T postgres sh -c 'PGPASSWORD="$KOMIKA_POSTGRES_PASSWORD" psql -X -w -h "$HOSTNAME" -U komika -d komika -Atc "SELECT count(*) FROM pg_ls_waldir()"'
```
