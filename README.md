# rustFUSE

Keep a directory, typically git repositories and their worktrees, in sync
across machines through rustFS or any S3-compatible object store. Every
machine works on a plain local folder at local speed, online or offline;
a small hub next to the store orders changes, and every machine resolves
conflicts the same way.

**Status:** Phases 1–4 done (sync core, multi-machine hub, git-aware
conflict handling, scale and speed). A 300k-file repository uploads in
about 15 s and downloads in about 35 s; `git status` with the fsmonitor
hook takes 0.06 s. See [docs/design.md](docs/design.md) for the
architecture and roadmap.

## Usage

```sh
export RUSTFUSE_ENDPOINT=http://rustfs:9000 RUSTFUSE_BUCKET=work
export RUSTFUSE_ACCESS_KEY=... RUSTFUSE_SECRET_KEY=...
export RUSTFUSE_HUB_TOKEN=some-shared-secret

# Next to rustFS:
rustfuse hub

# Optionally give each machine its own key instead of the shared token:
# limit it to volumes, revoke it on its own. Leave RUSTFUSE_HUB_TOKEN unset
# on the hub to accept keys only. On a machine, the key is its token.
rustfuse key create --name laptop --volume project   # prints rfk_..._...
rustfuse key list
rustfuse key revoke <id>

# First machine: upload an existing directory.
rustfuse init ~/src/project --volume project --hub rustfs-host
# Other machines: download it into an empty directory.
rustfuse attach ~/src/project --volume project --hub rustfs-host

# Over the internet, give the hub a certificate and use tls://. A
# self-signed certificate works if each machine gets a copy as --hub-ca:
#   openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
#     -keyout hub.key -out hub.pem -days 825 -subj /CN=rustfs-host \
#     -addext subjectAltName=DNS:rustfs-host
rustfuse hub --tls-cert hub.pem --tls-key hub.key
rustfuse attach ~/src/project --volume project --hub tls://rustfs-host --hub-ca hub.pem

# A machine that cannot reach rustFS, or should not hold its keys: leave out
# the bucket and the S3 settings, and it reads and writes through the hub
# with only the hub token.
env -u RUSTFUSE_BUCKET -u RUSTFUSE_ACCESS_KEY -u RUSTFUSE_SECRET_KEY \
  rustfuse attach ~/src/project --volume project --hub tls://rustfs-host --hub-ca hub.pem

# On every machine, keep it in sync (works offline, catches up later):
rustfuse sync ~/src/project
rustfuse status ~/src/project
rustfuse conflicts ~/src/project

# Next to the hub, e.g. daily: delete stored content nobody needs any more
# and compact packs that are mostly unused.
rustfuse gc --volume project

# Prometheus metrics (plain HTTP, GET /metrics): log position, outbox,
# uploads and errors on a machine; connections, appends and storage calls
# on the hub. Watch rustfuse_in_sync_timestamp_seconds for stalled machines.
rustfuse sync ~/src/project --metrics 127.0.0.1:9421
rustfuse hub --metrics 127.0.0.1:9420
```

For high availability, run a second `rustfuse hub` over the same bucket
(it needs conditional writes, which rustFS and AWS S3 have) and give
machines both: `--hub hub1,hub2`. See `docs/design.md` §9.1.

Build directories ignored by `.gitignore` (`target/`, `node_modules/`, …)
stay local; add a `.rustfuseignore` (gitignore syntax) for anything else.

### Mounting

A synced directory can also be mounted read/write somewhere else; the mount
keeps it in sync, and writes through it sync like edits in the directory:

```sh
# Linux, FUSE:
rustfuse mount ~/src/project /mnt/project
# macOS with macFUSE (build with `cargo install --path crates/rustfuse --features macfuse`);
# --fskit uses its FSKit backend, which needs no kernel extension:
rustfuse mount ~/src/project /Volumes/project --fskit
# Any OS with an NFS client (macOS needs nothing extra), or other machines:
rustfuse nfs ~/src/project --listen 127.0.0.1:20490
sudo mount -t nfs -o vers=3,tcp,port=20490,mountport=20490,locallocks 127.0.0.1:/ /Volumes/project   # macOS
sudo mount -t nfs -o vers=3,tcp,port=20490,mountport=20490,nolock 127.0.0.1:/ /mnt/project          # Linux
```

NFS has no authentication; listen on a trusted network only.

To avoid downloading a large volume up front, attach it lazily and use it
only through a mount: files keep their real size and date, and content is
fetched the first time something opens a file (git data always comes in
full, so `git status` and `git log` need no downloads):

```sh
rustfuse attach --lazy ~/.rustfuse/project --volume project
rustfuse mount ~/.rustfuse/project ~/src/project
```

Fetched files stay on disk. To cap how much they take, pass
`--cache-size` (for example `--cache-size 20G`) to `mount` or `nfs`: files
nobody opened in the last minute, and that have not been changed since
they were fetched, are emptied again, least recently used first, and
fetched once more when next opened.

### Git

- Commits, branches, stashes and worktrees replicate; `git fsck` stays clean.
- If the same branch moves on two machines at once and one side contains
  the other, the newer commit wins. If they diverged, one side stays on the
  branch and the other appears as `<branch>.conflict-<machine>-<time>`;
  merge it and it disappears. The worktree is put back to match the
  branch, so `git status` is clean.
- `rustfuse git-fsmonitor <repo>` makes `git status` ask the running
  `rustfuse sync` what changed instead of checking every file (0.06 s
  instead of 0.4 s on a 300k-file repository). It needs `rustfuse` on the
  `PATH` of every machine using the repository; without a running sync,
  git checks every file as before.
- rustfuse sets `core.checkStat=minimal` and `core.trustctime=false` in
  each repository so an index written on one machine stays valid on the
  others.

## Layout

| Crate | Purpose |
|---|---|
| `rustfuse-types` | IDs, clocks, tree entries, changesets |
| `rustfuse-store` | `ObjectStore` trait, S3 and in-memory implementations, conformance suite |
| `rustfuse-chunk` | Content-defined chunking and pack files |
| `rustfuse-hub` | The hub: per-volume ordered log, notifications |
| `rustfuse-sync` | The sync engine: scan, capture, apply, conflict rules, git pass |
| `rustfuse-mount` | FUSE and NFS frontends over a synced directory |
| `rustfuse` | CLI |

## Development

```sh
cargo test                                   # unit tests, in-memory store
docker compose -f dev/docker-compose.yml up -d
RUSTFUSE_TEST_S3_ENDPOINT=http://localhost:9000 cargo test   # + rustFS integration
RUSTFUSE_FUZZ=300 cargo test -p rustfuse-sync --test roundtrip random   # longer fuzz run
```

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
