# rustFUSE — Architecture & Design (v2)

Status: **Phases 0–4 implemented, Phase 5 in progress** · 2026-10-19

> **v2 changes the core model.** v1 (in git history) assumed every machine
> is online against a shared metadata server. The requirements below
> (offline work, the same worktree on several machines, git performance
> first) need a **local-first, replicated** design instead. Parts of v1
> carry over: the S3 layer, immutable content-addressed data, and the
> embedded `redb` index.

## 1. Requirements

Primary workload: **git repositories and worktrees** used from several
computers.

| # | Requirement | Source |
|---|---|---|
| R1 | Native-speed git: `git status`, checkout, rebase, `grep`, builds reading sources | user |
| R2 | Machines keep working **offline** and sync later | answer 1 |
| R3 | The **same worktree** can be edited from several machines | answer 2 |
| R4 | Machines may be on the LAN with rustFS **or remote** (VPN/internet) | answer 3 |
| R5 | Repos from 100 MB to 25 GB, no LFS today | answer 4 |
| R6 | Build output (`target/`, `node_modules/`, …) stays **local**, never synced | answer 5 |
| R7 | File watchers (IDE, dev servers) see changes made on other machines | answer 6 |
| R8 | Nothing is ever silently lost: concurrent changes are kept, and git state is merged in a way git understands | implied by R2+R3 |
| R9 | macOS and Linux first | user |

R2 alone rules out any design where reads or writes need the network: each
machine must hold a **full local replica** of the trees it works on. Given
that, the question is only how the replica is presented to programs and how
replicas converge.

## 2. Key decision: how the local replica is presented

| | **A. Native folder + sync daemon** | **B. FUSE/NFS mount over the local replica** |
|---|---|---|
| What the user sees | An ordinary directory on APFS/ext4/btrfs | A mount point served by rustFUSE |
| git speed | **Native.** Every `stat`/`open` hits the kernel's own FS | Every syscall crosses FUSE. Linux: ~2–5× slower on stat storms even with caching. macOS (NFS loopback / macFUSE): much slower on 100k-file repos |
| Offline | Trivial | Works (data is local) |
| Change capture | FS events (FSEvents / inotify / fanotify) + periodic verification scans | Exact, at the syscall |
| Lazy download of a 25 GB repo | No, the whole tree is materialized (needed for offline anyway) | Possible |
| Cross-machine `*.lock` exclusion while online | Not enforceable | Possible (intercept `O_EXCL` create) |
| IDE watchers on remote changes | Just work: sync writes real files, kernel emits events | Need explicit kernel invalidation; inotify on FUSE is limited |
| Complexity / risk | Lower; no kernel extension on macOS | Higher; macOS frontends are the weakest part |

**Recommendation: A**, a native folder kept in sync by a git-aware daemon.
It is the only option that meets R1 on macOS, and R2 already forces the
full local copy that B's lazy loading would save. B stays on the roadmap
(Phase 6) for very large trees and for "mount without syncing" use.

(The project name stays; FUSE becomes an optional frontend.)

## 3. Prior art

| Project | Model | Take-away for us |
|---|---|---|
| **Syncthing** | P2P, block-level, version vectors, conflict copies | Proven conflict-copy UX; no git awareness; P2P needs both peers online |
| **Mutagen** | Dev-focused 2-way sync over SSH/Docker, 3-way merge on file trees | Great for code; pairwise only, no hub, no git semantics |
| **Dropbox / iCloud** | Central hub, local-first, conflict copies | Known to corrupt `.git` when the same repo is touched on two machines; exactly what R8 must avoid |
| **Unison** | 3-way reconciliation against last common state | The reconciliation model we use per file |
| **Coda / AFS disconnected ops** | Op log replayed on reconnect | Op log + reintegration, conflicts surfaced to user |
| **JuiceFS / mountpoint-s3** | Online FUSE over S3 | Our v1 basis; still the model for optional Phase 6 mount |
| **git itself** | Content-addressed objects, refs as pointers | Objects never conflict; refs merge by ancestry; the index is a regenerable cache |

## 4. System overview

```
 Laptop (offline sometimes)        Desktop                     Remote box
┌───────────────────────────┐   ┌──────────────────────┐   ┌──────────────────────┐
│ ~/src/repo  (plain dir)   │   │ ~/src/repo           │   │ /work/repo           │
│   ▲ FS events   │ writes  │   │                      │   │                      │
│ rustfuse daemon           │   │ rustfuse daemon      │   │ rustfuse daemon      │
│  ├ local index (redb)     │   │  …                   │   │  …                   │
│  ├ outbox (ops not pushed)│   │                      │   │                      │
│  └ chunk cache            │   │                      │   │                      │
└───────┬───────────┬───────┘   └──────┬─────────┬─────┘   └──────┬─────────┬─────┘
        │ ops       │ chunks           │         │                │         │
        ▼           │                  ▼         │                ▼         │
   ┌─────────────────────────────┐               │                          │
   │ rustfuse-hub                │◀──────────────┴──────── ops ─────────────┘
   │ per-volume ordered op log,  │
   │ push notifications, auth    │
   └──────────────┬──────────────┘
                  │ log segments / snapshots      chunks / packs (direct)
                  ▼                                        ▼
   ┌─────────────────────────────────────────────────────────────────────┐
   │ rustFS / S3 bucket:  log/…   snap/…   pack/…                         │
   └─────────────────────────────────────────────────────────────────────┘
```

- **Volume**: one synced tree (e.g. a repo with its worktrees). Each machine
  attaches a volume to a local directory.
- **Daemon** (`rustfuse`): watches the directory, records changes as
  **operations**, uploads content, pushes ops to the hub, pulls others' ops
  and applies them to the directory, resolving conflicts.
- **Hub** (`rustfuse-hub`): a small server next to rustFS. Gives each
  volume a **totally ordered op log**, notifies connected daemons
  immediately, archives the log to the bucket, and authenticates
  machines. It never touches file content.
- **Bucket**: content (chunk packs), log segments and periodic snapshots.
  A new machine clones from snapshot + log tail.

Remote machines (R4) need the hub and the bucket reachable, directly or
through the hub acting as a storage proxy (§9): a machine set up without a
bucket sends its object reads and writes to the hub, which holds the S3
credentials.

## 5. Data model

### 5.1 Identity

- Every file and directory gets a **stable 128-bit node id** (machine id +
  counter) when first seen. Ops refer to node ids, not paths, so a rename on
  one machine and an edit on another compose correctly.
- Each machine has a **replica id**; ops carry a **hybrid logical clock**
  (HLC) timestamp, used for deterministic tie-breaks.

### 5.2 Operations

Two state-based ops, grouped into **changesets** (one per capture, all ops
sharing one version):

```
Upsert { node, entry, base: Option<Versions>, changed: {loc, data, meta} }
Remove { node, base: Versions }
entry    = { parent, name, mode, mtime, content }   uid/gid are not synced
Versions = { loc, data, meta }, each a Version = (HLC time, replica)
```

Each node keeps a **version per part**: location (parent + name), data
(content + mtime) and meta (permission bits). An op names the parts it
changes and the versions its author saw (`base`), so the receiver can tell a
clean change from a concurrent one per part, and a rename on one machine
composes with an edit on another (§7.2). Applying the same changeset twice
changes nothing.

The hub assigns each accepted changeset a sequence number. A machine's
state is the log "applied up to seq N", plus its own **outbox** of
changesets not yet seen in the log; its disk shows the log with the outbox
replayed on top. Offline work just grows the outbox; reconnecting sends it,
and the hub accepts it as-is (the hub does not resolve conflicts: every
machine resolves deterministically from the same ordered log, so all
replicas converge to the same result).

### 5.3 Content

- Files are split with **content-defined chunking** (FastCDC, ~64 KiB
  average) and hashed with **BLAKE3**. Identical chunks are stored once
  across files, versions and machines. A small edit to a large file uploads
  only the changed chunks.
- Files ≤ 256 bytes are stored inline in the op (refs, `HEAD`, worktree
  pointers). Inline data lives in every tree, index and snapshot, so a
  larger threshold costs memory on trees with many small files (§8.1).
- Chunks are batched into **pack objects** (~8–32 MiB) per push, with an
  index, so a 100k-file repo is not 100k S3 PUTs. Packs are immutable;
  cleanup deletes unused ones and compacts sparse ones (§5.5).
- Optional zstd compression and client-side encryption per chunk.
- Each daemon keeps a local **chunk cache** so applying remote ops for
  content it already has costs nothing.

### 5.4 Local index

The daemon's `redb` database holds, per volume: node table (id → parent,
name, kind, mode, version, size, mtime, content hash, inode/dev for fast
change detection), last applied seq, outbox, conflict list, and the
watcher's bookmark (FSEvents event id / last scan).

### 5.5 Cleanup

`rustfuse gc --volume <name>` (next to the hub, e.g. daily) deletes packs
no machine can need. A pack is **unused** when no file in the current state
and no changeset from the last 14 days (`--retention-days`) points into it,
and it was written more than 14 days ago. Deletion takes two runs:

1. A run writes a tombstone `gc/dead/<pack>` for each unused pack.
2. A later run, at least an hour after, deletes the pack and its tombstone
   if it is still unused, or drops the tombstone if something uses it
   again. Old snapshots are deleted too (the latest two stay).

The danger is a machine that was offline for longer than the retention and
holds unsent changesets pointing at such packs: a copy of a file others
deleted, a rename of it, or new content that matched old chunks and so was
never uploaded. Before sending, a machine checks that every pack its unsent
changesets use exists and has no tombstone (a pack checked in the last five
minutes is trusted). If one is gone or marked, it forgets those chunks,
re-reads the files and uploads them again, and points the changesets at the
new copies; an intermediate version that no longer exists on disk is
replaced by the file's current content (the end result is the same). A
changeset checked before a tombstone appeared is in the log well within
the hour, so the deleting run sees it and keeps the pack.

A machine catching up applies the whole log before it updates the disk, so
it never fetches files that are gone again by the time it is up to date.

**Compaction.** A pack written before the retention window whose used
chunks fill less than half of it (and at least 1 MiB is unused) is
compacted: its used chunks are copied, verified, into a new pack, a
forwarding table `fwd/<pack>` records where each chunk went (by its old
offset), and only then the old pack gets a tombstone like an unused one.
Nothing that names the old pack is rewritten: readers that find it gone
follow the table (up to 8 hops, for a pack compacted again), cleanup counts
the new pack as used, and a machine checking its unsent changes points them
at the new place. A table is dropped once its pack is gone and nothing
names it directly. Measured on a 700 MB repository (24k loose objects,
then `git gc`): the bucket held 199 MiB after syncing the gc; one run
compacted 7 packs (112 MiB into 30 MiB) in 1.4 s, and the next deleted
the originals, leaving 117 MiB.

## 6. Change capture and apply

**Capture (local → ops).**
1. FS events (FSEvents on macOS, inotify on Linux; fanotify where allowed)
   mark paths dirty. Events are only hints.
2. A debounced worker (default 100–300 ms after the last event) stats
   dirty paths, compares to the index (size, mtime, inode, ctime), hashes
   changed files, and emits ops. Renames are detected by inode identity
   (same dev/inode, new path).
3. A periodic and startup **verification scan** catches anything events
   missed (overflow, daemon not running). It is a parallel stat walk that
   only hashes files whose stat changed.
4. Files still being written (mtime within the debounce window, or open for
   write where detectable) are retried rather than synced half-written.

**Apply (remote ops → disk).**
- Content is written to a temp file in the same directory and `rename`d
  into place (atomic for readers, and generates normal FS events for
  watchers, which covers R7).
- The daemon tags its own writes so the watcher does not echo them back as
  new ops.
- Ops apply in log order; a batch that touches `.git` applies **refs last**
  (after the objects they point to exist), so git never sees a ref to a
  missing object.

**Exclusions (R6).** Built-in: `.rustfuse/`, `.DS_Store`, and in any
repository's `.git/`: `**/*.lock`, `gc.pid`, `fsmonitor--daemon/` and
`worktrees/*/gitdir` (each machine's own pointer to its copy of a linked
worktree, §7.1). Then `.rustfuseignore` at the root (gitignore syntax, `!`
re-includes). Then directories the repository's own `.gitignore` files and
`.git/info/exclude` exclude, which is how `target/`, `node_modules/` and
friends stay local without a hard-coded list. Files matched by
`.gitignore` still sync, because repositories often track them
(`Cargo.lock`). Excluded paths stay purely local.

## 7. Conflict resolution

All daemons see the same ordered log and resolve the same way, so every
replica converges. Rules, from most to least specific:

### 7.1 Git-aware rules (`.git/` and worktree gitdirs)

The log applies the generic rules (§7.2) everywhere, so a conflict inside
`.git/` first produces an ordinary conflict copy. After each sync round
every machine runs a **git pass** (`rustfuse-sync/src/git.rs`) over its own
disk that turns those copies into git-correct results. Each fix depends
only on what the log produced, so machines doing it at the same time write
the same bytes and their changes merge as "same content". It needs the
`git` binary; without it the copies simply stay.

| Path (in a git dir) | Rule |
|---|---|
| `objects/**` | Content-addressed: same name, same object. Copies (e.g. a loose object compressed differently, `commit-graph`) are dropped. |
| `refs/**`, `HEAD`, `worktrees/*/HEAD` | If one commit is an **ancestor** of the other, the descendant wins (via `git update-ref` with the old value, so a concurrent local git command is never overwritten) and the copy is dropped. If they diverged, the later (HLC) value stays and the other remains as the branch `<name>.conflict-<machine>-<time>`; once it is merged into the branch, the copy is dropped. A HEAD moved to different branches keeps the later checkout. |
| `packed-refs` | Merged per ref with the same ancestry rule; refs on only one side are kept; a diverged ref is written as a loose `<ref>.conflict-…`. Written under `packed-refs.lock` like git. |
| `logs/**` (reflogs) | Union of both, ordered by timestamp. |
| `index`, `worktrees/*/index` | Keep whichever version agrees with HEAD on more entries, so a `git status` refresh on one machine never undoes a commit, checkout or merge made on another. Nothing in the worktree is lost either way. |
| `FETCH_HEAD`, `ORIG_HEAD`, `AUTO_MERGE`, `COMMIT_EDITMSG`, `gc.log` | Transient; copies dropped. |
| `*.lock`, `gc.pid`, `fsmonitor--daemon/` | Never synced. |
| `config`, hooks, others | Generic rules (§7.2). |

**The worktree after a branch conflict.** Files a worktree holds follow
their own last write, so after two machines committed to the same branch
the worktree mixes both commits. When the checked-out branch is resolved
in favour of commit T over commit X, the pass looks at the paths that
differ between X and T and, for each, puts the file back to T's version
only if it holds exactly X's committed version (and resets the index entry
if only the index does). A conflict copy next to such a file that holds
X's or T's committed version is removed, since git keeps it. Uncommitted
edits are never touched, and nothing runs while a merge, rebase,
cherry-pick or revert is in progress. Each diverged conflict is cleaned
up once per machine (remembered in `.rustfuse/git-handled`); merging the
conflict branch later never triggers it again.

**Linked worktrees.** `git worktree add` writes absolute paths in two
places. The worktree's `.git` file is rewritten to a relative path, which
every git version follows, so it is correct on every machine. The back
pointer `.git/worktrees/<id>/gitdir` is not synced; each machine writes its
own, so `git worktree list` and `prune` see the right path everywhere.

**Index settings.** Each repository gets `core.checkStat=minimal` and
`core.trustctime=false` (only when unset). File mtimes and sizes sync
exactly, but inode numbers and ctimes cannot, so without these an index
written on one machine would make `git status` rehash every file on the
next.

### 7.2 Generic rules (worktree files and everything else)

| Situation | Result |
|---|---|
| Edit vs edit, different content | Later (HLC) edit stays at the path; the other is saved next to it as `name.conflict-<machine>-<time>.ext` and listed by `rustfuse conflicts`. In git, the conflict copy shows up as untracked, which makes it visible. |
| Edit vs edit, same content | No conflict. |
| Edit vs delete | **Edit wins**; the file is restored. |
| Rename vs edit | Both apply (ops address node ids). |
| Rename vs rename (same node) | Later rename wins. |
| Two creates at the same path | Later stays; the other gets a conflict name. |
| Rename that would create a directory cycle | Later rename is converted to a move into `/.rustfuse-conflicts/`. |
| Delete dir vs create inside it | Directory kept with the new content. |

### 7.3 Online, same worktree at the same moment

When machines are online, ops propagate in roughly 100–500 ms (debounce +
push + notify + apply). Two people running git **in the same worktree at the
same instant** on two machines can still race, because folder mode cannot
make `.git/index.lock` exclusive across machines. The rules above keep
every result recoverable. `rustfuse status` shows "worktree active on
<machine>" when another machine has changed the worktree in the last N
seconds, and an optional git hook (`pre-commit`, `post-checkout`) warns
before you commit over a concurrent session.

## 8. Performance plan

| Path | Approach |
|---|---|
| git on the tree | Native FS, no overhead (§2) |
| Change detection on 500k-file trees | Event-driven; verification scans are parallel `stat` walks comparing (mtime, ctime, size, inode) and only hash changed files |
| `git status` | **fsmonitor hook** (`rustfuse git-fsmonitor <repo>`): the daemon journals every changed path from its watcher and answers git's `core.fsmonitor` hook (protocol v2) over a socket in `.rustfuse/`. Before answering it writes a cookie file and waits for its event, so every change made before the query is included. Tokens name the daemon run; a token from another run or another machine's index, a dropped event or a trimmed journal answers "everything changed" and git scans as usual. Paths inside `.git` are left out, so a `git status` that changed nothing returns the same token and does not rewrite (and re-sync) the index. On Linux, where git has no built-in fsmonitor, this is a large win |
| Initial clone of 25 GB | Parallel pack downloads (bounded concurrency, ranged GETs), chunk dedupe, write files in parallel, set mtimes, then hand off to the watcher |
| Upload | Chunk + hash in parallel (rayon), batch into packs, many concurrent PUTs |
| Remote links | Packs compress well; ops are tiny; resumable transfers |
| Propagation latency | Hub pushes notifications over a persistent connection; no polling |

### 8.1 Measurements

A synthetic git repository with 312k entries (300k text files, 700 MB, one
commit), two machines on one host against rustFS in docker:

| | Phase 3 | Phase 4 so far |
|---|---|---|
| `init` (scan, upload) | 35–74 s | 14–15 s |
| `attach` (download, write) | 60–72 s | 32–34 s |
| No-op full rescan | 4.2–4.8 s | 2.6–3.0 s |
| One edited file reaches the other machine | 0.15–0.25 s | 0.16 s |
| 2000-file commit reaches the other machine's `HEAD` | 2.1 s | 0.46 s |
| Daemon memory (RSS) | ~2 GB | ~0.7 GB |
| Hub memory | 516 MB | 127 MB |
| Local index (`.rustfuse/`) | 0.9–1.3 GB | 210–270 MB |
| `git status`, repeated | 0.33–0.5 s | 0.06 s (fsmonitor hook) |
| Branch diverged on two machines, 2000 files each, sync after | 13–16 s | 0.5–0.7 s |

What changed: inline threshold 4 KiB → 256 bytes; files are written in
parallel on apply (directories first, permissions last); the index rewrites
only nodes that actually changed after a catch-up instead of every pending
node, and clears the pending table by dropping it.

A larger repository (4.6 GB: a 2.3 GB pack, a 1 GB binary, 11k files):
`init` 15 s (peak 0.7 GB RAM), `attach` 16 s (peak 0.4 GB; it was 2.4 GB
before large files were streamed), no-op sync 0.2 s. A 1 MiB edit in the
middle of the 1 GB file makes the other machine fetch 1.4 MB, not 1 GB:
files of 64 MiB or more are written a 64 MiB window at a time, and chunks
the old copy on disk already holds are read from it (checked by hash)
instead of downloaded. 25 GB was not tried: the test machine's disk holds
about two copies of 5 GB plus the bucket.

With the fsmonitor hook, the first `git status` after the index arrived
from another machine still checks every file (that machine's token means
nothing here), then later ones take 0.06–0.1 s.

## 9. Hub

- Rust, tokio; `rustfuse hub` runs next to rustFS. One hub serves every
  volume in a bucket, and several hubs can serve the same bucket at once
  (§9.1).
- Per volume: the hub numbers each changeset and writes it to the bucket
  (`log/{seq}`) **before** acknowledging it, then streams it to every
  connected machine. Recent changesets are cached in memory; older ones
  are read back from the bucket. A restarted or replacement hub continues
  from the bucket alone.
- Snapshots (`snap/{seq}`) are written by the machine whose changeset lands
  at seq 1 or a multiple of 256; new machines attach from the latest
  snapshot plus the log tail, straight from the bucket.
- Protocol: TCP, length-prefixed zstd + postcard frames. `Hello{volume,
  token, after}` subscribes from a position; `Push(changeset)` appends.
- Storage proxy (Phase 5): a connection that opens with `Store{volume,
  token}` instead carries object calls (get, ranged get, put, head,
  delete, list) for that volume's keys, many in flight at once, each
  answered by id. These frames are plain postcard (the chunks are already
  zstd-compressed). `rustfuse init/attach` without `--bucket` set this up
  and remember it (`via_hub` in the config), so such a machine needs only
  the hub address and token. On the 4.6 GB repository (§8.1), attaching
  through the hub took 12.4 s against 10.9 s direct, with 190 MB peak on
  the hub.
- Auth: one shared token per hub, from `RUSTFUSE_HUB_TOKEN`; it also grants
  storage access to every volume behind the hub. Per-machine keys may come
  later in Phase 5.
- TLS (Phase 4): `rustfuse hub --tls-cert hub.pem --tls-key hub.key`;
  machines use `--hub tls://host[:port]`. They trust the system's roots, or
  the certificates in `--hub-ca` (kept in the machine's config). A
  certificate in `--hub-ca` is also accepted as the exact certificate the
  hub presents, like a pinned SSH host key, so the self-signed certificate
  `openssl req -x509` makes works as is; through a CA, the certificate must
  name the host. rustls with the aws-lc-rs provider, already used by the
  S3 client. Without a certificate the hub speaks plain TCP: keep it on a
  trusted network or a tunnel.
- Metrics (Phase 5): `rustfuse hub --metrics addr` and `rustfuse sync
  --metrics addr` serve Prometheus text over plain HTTP (`GET /metrics`),
  from atomics with no extra dependencies. The hub reports open and total
  connections by kind (`log`, `store`), refused connections, appends per
  volume with their latency, each volume's log head, slow machines
  dropped, and storage-proxy calls by op with latency, errors and bytes in
  and out. A machine reports whether it is connected, its applied position
  against the log head it last heard of, its outbox length, the last time
  it was fully in sync (connected, caught up, nothing to send), what it
  pushed, received, captured, read and uploaded, capture and apply
  latency, and failures by stage. Lag is `rustfuse_hub_head_seq -
  rustfuse_applied_seq`; a machine whose
  `rustfuse_in_sync_timestamp_seconds` stops moving needs a look. Bind it
  to localhost or a trusted network: it has no auth.
- HA is optional (§9.1); if every hub is down, every machine keeps working
  offline, and content waits in a local spool if the bucket is unreachable
  too.

### 9.1 High availability

The bucket holds all shared state; a hub keeps only a cache of recent
changesets and its connections. So HA means running more than one hub
over the same bucket and letting machines reach whichever is up.

- **Several hubs, one log.** A hub writes `log/{seq}` with a create-only
  write (S3 `If-None-Match: *`), so exactly one hub can claim each number.
  A hub that loses the race reads what the other wrote, sends it to its
  own machines, and tries the next number. While machines are connected,
  each hub also lists the log every 2 s to pick up the other hubs'
  appends. Machines on the same hub see a change at once; across hubs it
  takes up to those 2 s. `rustfuse_hub_peer_changesets_total` counts what
  a hub picked up from the others.
- **Store support.** This needs conditional writes. AWS S3 and rustFS
  support them (the store conformance suite races eight writers and checks
  exactly one wins; CI runs it against rustFS). On a store without them,
  run one hub at a time.
- **Machines.** `--hub` takes a comma-separated list (`tls://hub1,hub2`).
  Every connect, including the storage proxy's, tries each address in
  order with a 5 s timeout. Give each machine the nearest hub first. A DNS
  name or TCP load balancer in front of the hubs works as well. All hubs
  must share `RUSTFUSE_HUB_TOKEN`, and with TLS each hub's certificate must
  name its own host (or be listed in `--hub-ca`).
- **What failover costs.** A machine whose hub goes away logs the lost
  connection, reconnects to the next address with the usual backoff
  (0.5 s, doubling), re-sends anything not yet acknowledged, and catches
  up from its last applied changeset. A changeset that a hub wrote but
  whose acknowledgement was lost is re-sent. The receiving hub recognises
  it among the changesets it has cached (the last 4096, at least the last
  64 right after it opens the volume) and answers with the number it
  already has, so it is not appended twice.
- **Upgrades.** Hubs hold no state, so restart them one at a time.
- **The bucket is the single point of failure.** Make rustFS itself
  redundant (multiple nodes with erasure coding) and back up or replicate
  the bucket. While the bucket is down, machines keep working, and
  machines that reach it directly spool new packs locally until it is
  back. Machines that reach it through a hub keep their changes in the
  outbox, and their uploads wait the same way.
- **Cleanup.** Run `rustfuse gc` from one place on a schedule, not from
  every hub.
- **Tested.** Two hubs over rustFS, two machines (one through the hub's
  storage proxy) listing them in opposite order:
  - With both hubs up, 20 files created on each machine at once converge.
  - With the first hub killed, both machines move to the second, and new
    edits (including a 300 KB file) converge.
  - With the first hub restarted and the second killed, they move back,
    and again everything converges.

## 10. Code layout

```
crates/
  rustfuse-types   ids, HLC, versions, ops, changesets, snapshots
  rustfuse-store   ObjectStore trait + S3 + memory + prefix views
  rustfuse-chunk   FastCDC + BLAKE3 + packs
  rustfuse-hub     log in the bucket, wire protocol, hub server + client
  rustfuse-sync    tree + conflict rules, redb index, scanner, watcher,
                   applier, outbox, sync loop
  rustfuse         CLI: hub, init, attach, sync, status, conflicts, bench
  rustfuse-fuse    optional mount frontend (Phase 6)
```

## 11. Testing

- Unit and property tests for chunking, op application and every conflict
  rule.
- **Convergence tests**: N simulated replicas apply random concurrent op
  sequences in different interleavings with network partitions, then
  assert byte-identical trees on all replicas and that no content was lost.
- **Git scenario tests**: scripted real git on 2–3 replicas: commit on both
  offline, rebase on one, branch on another, gc on a third, reconnect;
  then `git fsck` must be clean and every commit reachable from a ref or a
  conflict ref.
- Integration against rustFS in CI (Linux and macOS runners).
- Benchmarks: initial clone and steady-state sync of real repos (linux
  kernel, a 25 GB monorepo-like fixture), propagation latency, `git
  status` with and without fsmonitor.

## 12. Phased plan

| Phase | Deliverable |
|---|---|
| **0. Foundations** | Workspace, CI, rustFS harness, S3 layer (**done**) |
| **1. Single machine** (**done**) | Chunking + packs, local index, watcher + scanner, `rustfuse init` (upload a tree), `rustfuse attach` (materialize a tree on another dir), one-way sync. This already gives backup and restore. |
| **2. Hub + multi-machine** (**done**) | Hub, op log, HLC, outbox, push/pull, live notifications, generic conflict rules, convergence test harness |
| **3. Git awareness** (**done**) | Objects union, ref ancestry merge, packed-refs, reflogs, index handling, worktree cleanup after branch conflicts, portable linked worktrees, exclusions, git scenario tests |
| **4. Scale & speed** (**done**) | Measured on a 300k-file repo (§8.1); parallel apply, leaner index, fsmonitor hook, batched git cleanup, hub TLS, streamed large files with local chunk reuse (**done**); chunks are zstd-compressed since Phase 1. Deferred: client-side encryption (rustFS server-side encryption for now); a full 25 GB run |
| **5. Remote & ops** (in progress) | Hub as storage proxy for remote machines (**done**), cleanup of unused packs and old snapshots, compaction of sparse packs (**done**), metrics (**done**), several hubs per bucket with failover (**done**) |
| **6. Optional mount** | FUSE/NFS frontend over the replica with lazy hydration (v1 ideas) |

## 13. Decisions and open questions

Decided (2026-10-19):

1. **Presentation (§2)**: plain local folder plus daemon first; an optional
   FUSE mount later (Phase 6).
2. **Hub placement**: `rustfuse-hub` runs next to rustFS.
3. **Conflicting edits to the same file**: a sibling conflict copy
   (`name.conflict-<machine>-<time>.ext`). An automatic 3-way text merge
   can be added later for text files.

Open:

- The git pass runs after a conflict arrives. Two people running git in
  the **same worktree at the same instant** on two machines can still
  interleave (§7.3); results stay recoverable from refs, reflogs and
  conflict copies.
- A 3-way text merge for ordinary files (instead of a conflict copy) is a
  possible later addition.

