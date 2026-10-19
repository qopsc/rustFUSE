# rustFUSE

Keep a directory, typically git repositories and their worktrees, in sync
across machines through rustFS or any S3-compatible object store. Every
machine works on a plain local folder at local speed, online or offline;
a small hub next to the store orders changes, and every machine resolves
conflicts the same way.

**Status:** Phases 1–3 done (sync core, multi-machine hub, git-aware
conflict handling). See [docs/design.md](docs/design.md) for the
architecture and roadmap.

## Usage

```sh
export RUSTFUSE_ENDPOINT=http://rustfs:9000 RUSTFUSE_BUCKET=work
export RUSTFUSE_ACCESS_KEY=... RUSTFUSE_SECRET_KEY=...
export RUSTFUSE_HUB_TOKEN=some-shared-secret

# Next to rustFS:
rustfuse hub

# First machine: upload an existing directory.
rustfuse init ~/src/project --volume project --hub rustfs-host
# Other machines: download it into an empty directory.
rustfuse attach ~/src/project --volume project --hub rustfs-host

# On every machine, keep it in sync (works offline, catches up later):
rustfuse sync ~/src/project
rustfuse status ~/src/project
rustfuse conflicts ~/src/project
```

Build directories ignored by `.gitignore` (`target/`, `node_modules/`, …)
stay local; add a `.rustfuseignore` (gitignore syntax) for anything else.

### Git

- Commits, branches, stashes and worktrees replicate; `git fsck` stays clean.
- If the same branch moves on two machines at once and one side contains
  the other, the newer commit wins. If they diverged, one side stays on the
  branch and the other appears as `<branch>.conflict-<machine>-<time>`;
  merge it and it disappears. The worktree is put back to match the
  branch, so `git status` is clean.
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
