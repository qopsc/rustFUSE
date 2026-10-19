# rustFUSE — Architecture & Design (v2)

Status: **draft for review** · 2026-10-19

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
through the hub acting as an S3 proxy (Phase 5).

## 5. Data model

### 5.1 Identity

- Every file and directory gets a **stable 128-bit node id** (machine id +
  counter) when first seen. Ops refer to node ids, not paths, so a rename on
  one machine and an edit on another compose correctly.
- Each machine has a **replica id**; ops carry a **hybrid logical clock**
  (HLC) timestamp, used for deterministic tie-breaks.

### 5.2 Operations

```
Create  { node, parent, name, kind, mode, content? | symlink target }
Write   { node, base_version, content }          content = chunk list + size
Rename  { node, base_parent, base_name, parent, name }
Delete  { node, base_version }
SetMeta { node, mode?, mtime? }                   uid/gid are not synced
```

Every node carries a **version** = (replica, counter) of the last op applied
to it. An op's `base_version` says which version the author saw; if the
receiver's current version differs, the op is **concurrent** and goes
through conflict resolution (§7).

The hub assigns each accepted op a sequence number. A daemon's state is
"applied up to seq N" plus its own unpushed outbox. Offline work just grows
the outbox; reconnecting pushes it, and the hub accepts it as-is (the hub
does not resolve conflicts: every daemon resolves deterministically from
the same ordered log, so all replicas converge to the same result).

### 5.3 Content

- Files are split with **content-defined chunking** (FastCDC, ~64 KiB
  average) and hashed with **BLAKE3**. Identical chunks are stored once
  across files, versions and machines. A small edit to a large file uploads
  only the changed chunks.
- Files ≤ 4 KiB are stored inline in the op (most refs, small configs).
- Chunks are batched into **pack objects** (~8–32 MiB) per push, with an
  index, so a 100k-file repo is not 100k S3 PUTs. Packs are immutable;
  compaction (Phase 5) rewrites sparse packs.
- Optional zstd compression and client-side encryption per chunk.
- Each daemon keeps a local **chunk cache** so applying remote ops for
  content it already has costs nothing.

### 5.4 Local index

The daemon's `redb` database holds, per volume: node table (id → parent,
name, kind, mode, version, size, mtime, content hash, inode/dev for fast
change detection), last applied seq, outbox, conflict list, and the
watcher's bookmark (FSEvents event id / last scan).

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

**Exclusions (R6).** Defaults: `target/`, `node_modules/`, `.venv/`,
`build/`, `dist/`, `.DS_Store`, editor swap files, `.git/*.lock`,
`.git/gc.pid`, `.git/fsmonitor--daemon/`. Configurable per volume
(`.rustfuseignore`, gitignore syntax). Excluded paths stay purely local.

## 7. Conflict resolution

All daemons see the same ordered log and resolve the same way, so every
replica converges. Rules, from most to least specific:

### 7.1 Git-aware rules (`.git/` and worktree gitdirs)

| Path | Rule |
|---|---|
| `objects/**` (loose + packs), `lfs/objects/**` | Content-addressed and immutable. Same name means same content, so take the **union**. Never a conflict. |
| `refs/**`, `HEAD`, `packed-refs` (per ref line) | If one value is an **ancestor** of the other (checked against the local object graph), take the descendant (a fast-forward). Otherwise keep the later (HLC) value and record the other as `refs/rustfuse-conflicts/<machine>/<ref>` so `git log`/`git merge` can recover it. |
| `logs/**` (reflogs) | Append-only, so merge by **union**, ordered by timestamp. |
| `index` | A cache plus staged state. Keep the later version and save the other as `index.rustfuse-conflict-<machine>`; `rustfuse conflicts` explains how to restore staged changes. git rescans the worktree, so nothing in the worktree is lost. |
| `*.lock`, `gc.pid`, `fsmonitor--daemon/` | Never synced. |
| `config`, `worktrees/*/` admin files, others | Generic rules (§7.2). |

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
| `git status` | Optional **fsmonitor** integration: the daemon already knows what changed, so it answers git's `core.fsmonitor` hook (v2 protocol). On Linux, where git has no built-in fsmonitor daemon, this is a large win |
| Initial clone of 25 GB | Parallel pack downloads (bounded concurrency, ranged GETs), chunk dedupe, write files in parallel, set mtimes, then hand off to the watcher |
| Upload | Chunk + hash in parallel (rayon), batch into packs, many concurrent PUTs |
| Remote links | Packs compress well; ops are tiny; resumable transfers |
| Propagation latency | Hub pushes notifications over a persistent connection; no polling |

## 9. Hub

- Rust, tokio; one process per deployment, run next to rustFS.
- Per volume: append-only op log in embedded `redb`, archived to the bucket
  as immutable segments plus periodic snapshots. The hub can be rebuilt
  from the bucket.
- API (gRPC or a lean framed protocol over TLS): `push(volume, ops) → seqs`,
  `subscribe(volume, from_seq)` stream, `snapshot`, `auth`.
- Auth: per-machine keys, revocable.
- HA is optional; if the hub is down, every machine keeps working offline.

## 10. Code layout

```
crates/
  rustfuse-types   ids, HLC, ops, errors            (exists)
  rustfuse-store   ObjectStore trait + S3 + memory  (exists, Phase 0)
  rustfuse-chunk   FastCDC + BLAKE3 + packs
  rustfuse-index   local redb index, outbox, conflicts
  rustfuse-watch   FSEvents / inotify / scanner
  rustfuse-sync    capture, apply, conflict rules (generic + git)
  rustfuse-hub     hub server
  rustfuse         CLI + daemon: init, attach, status, conflicts, bench
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
| **1. Single machine** | Chunking + packs, local index, watcher + scanner, `rustfuse init` (upload a tree), `rustfuse attach` (materialize a tree on another dir), one-way sync. This already gives backup and restore. |
| **2. Hub + multi-machine** | Hub, op log, HLC, outbox, push/pull, live notifications, generic conflict rules, convergence test harness |
| **3. Git awareness** | Objects union, ref ancestry merge, packed-refs, reflogs, index handling, exclusions, `rustfuse conflicts`, git scenario tests |
| **4. Scale & speed** | 25 GB / 500k-file repos, parallel clone, fsmonitor hook, compression, encryption |
| **5. Remote & ops** | Hub as S3 proxy for remote machines, pack compaction + GC, metrics, HA notes |
| **6. Optional mount** | FUSE/NFS frontend over the replica with lazy hydration (v1 ideas) |

## 13. Open questions

1. **Presentation (§2)**: native folder + daemon (recommended) or a FUSE
   mount?
2. **Hub placement**: run `rustfuse-hub` next to rustFS (recommended), or
   must it be serverless (log in the bucket only, polling for changes)?
3. **Conflict copies for worktree files**: is a sibling
   `name.conflict-…` file acceptable, or would you rather get an automatic
   3-way text merge with conflict markers for text files?
