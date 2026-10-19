# rustFUSE — Architecture & Design

Status: **draft for review** · 2026-10-19

## 1. Goal

Mount rustFS (or any S3-compatible object store) as a native filesystem on
macOS and Linux that is:

1. **Fast**: high sequential throughput (saturate the NIC on large files) and
   low latency for metadata-heavy work (`ls`, `stat`, `git status`, builds).
2. **Multi-writer consistent**: several machines mount the same filesystem at
   the same time, and see each other's changes with well-defined, POSIX-like
   semantics (atomic rename, working locks, close-to-open at minimum).
3. **Portable**: Linux and macOS first, without forcing users to disable
   macOS security to load a kernel extension.

Non-goals for v1: Windows, being a general distributed object store,
replacing the object store's own durability/replication.

## 2. Prior art and what we take from it

| Project | Layout | Metadata | Multi-writer | Notes |
|---|---|---|---|---|
| **s3fs-fuse**, **goofys** | 1 file = 1 object | the bucket itself (LIST/HEAD) | no real coordination | Bucket stays human-readable, but random writes rewrite whole objects, rename = copy+delete, `ls` = LIST (slow). goofys trades POSIX for speed. |
| **rclone mount** | 1 file = 1 object | bucket + local VFS cache | single writer | Good caching modes; consistency is "last upload wins". |
| **mountpoint-s3** (AWS, Rust) | 1 file = 1 object | bucket | no (new files only, no modify/rename) | Proves Rust + `fuser` + parallel ranged GETs can saturate 100 Gb/s. Great read-path reference. |
| **JuiceFS** | chunks → slices → blocks (immutable) | external DB (Redis, TiKV, Postgres, SQLite…) | yes, close-to-open | Closest to our goals. Full POSIX, atomic rename, locks. Clients poll/expire metadata caches rather than receiving invalidations. Go. |
| **ZeroFS** (Rust) | LSM (SlateDB) on S3 | in the same LSM | single writer | NFS/9P frontends, nice macOS story; single-writer by design. |
| **CephFS / Lustre / NFSv4** | — | dedicated metadata servers | yes, via capabilities/leases/delegations | The model for *low-latency* consistency: clients cache aggressively under server-granted leases that the server revokes on conflict. |

**Takeaways**

- Mapping files 1:1 to objects cannot deliver random writes, atomic rename,
  fast `ls`, or multi-writer consistency. S3 has no rename, no append, no
  partial update, and (on many S3-compatibles) no reliable conditional writes.
- Every system that meets our three goals splits **metadata** (small, hot,
  transactional) from **data** (large, immutable, in the object store).
- Lowest metadata latency comes from **server-granted leases with push
  invalidation** (Ceph caps / NFSv4 delegations), not TTL polling.

## 3. Key decision: storage format

**Recommendation: chunked, immutable data blocks in the bucket plus a separate
metadata service** (JuiceFS-style layout, Ceph-style cache coherence).

Consequence the user should accept explicitly: files are **not** browsable as
plain objects with other S3 tools; the bucket holds rustFUSE blocks. To soften
this we plan (Phase 6) a read-only *passthrough* mount for existing plain
buckets and an `import`/`export` command.

## 4. System overview

```
 Machine A                 Machine B                 Machine C
┌──────────────┐          ┌──────────────┐          ┌──────────────┐
│ app          │          │ app          │          │ app          │
│  ↓ kernel    │          │  ↓ kernel    │          │  ↓ kernel    │
│ FUSE / NFS   │          │ FUSE / NFS   │          │ FUSE / NFS   │
│ rustfuse     │          │ rustfuse     │          │ rustfuse     │
│  ├ VFS core  │          │  ├ VFS core  │          │  ├ VFS core  │
│  ├ meta cache│◀─leases──┼──┼────────────┼──────────┼──┤           │
│  └ blk cache │  invalid.│  └ blk cache │          │  └ blk cache │
└──────┬───┬───┘          └──────┬───┬───┘          └──────┬───┬───┘
       │   └──────── metadata RPC (TCP) ──┐                │   │
       │                 ┌────────────────▼───────────┐    │   │
       │                 │ rustfuse-metad             │◀───┼───┘
       │                 │ inodes, dirs, chunk maps,  │    │
       │                 │ sessions, leases, locks    │    │
       │                 │ embedded KV + WAL          │    │
       │                 └────────────┬───────────────┘    │
       │      block GET/PUT           │ snapshots / WAL    │
       ▼                              ▼                    ▼
┌─────────────────────────────────────────────────────────────────┐
│            rustFS / S3-compatible bucket                         │
│   data/…  (immutable blocks)      meta/…  (snapshots, WAL)       │
└─────────────────────────────────────────────────────────────────┘
```

Three components:

1. **Client (`rustfuse mount`)**: kernel frontend (FUSE or NFS loopback), a
   frontend-agnostic VFS core, a metadata cache under leases, a memory + local
   disk block cache, and an S3 data path.
2. **Metadata server (`rustfuse-metad`)**: the single source of truth for the
   namespace. Linearizable operations, grants and revokes client leases, and
   owns POSIX/BSD locks. Persists to an embedded KV store and backs itself up
   to the bucket.
3. **Object store**: holds immutable data blocks and metadata
   snapshots. Any S3-compatible service; rustFS is the primary test target.

## 5. Data layout

Borrowed from JuiceFS because it is proven and makes every object immutable:

- A file is split into fixed **chunks** of 64 MiB of logical address space.
- Each write produces a **slice**: an immutable extent with a globally unique
  64-bit `slice_id` (allocated in ranges from metad). A chunk's content is the
  ordered list of slices covering it; later slices shadow earlier ones.
- A slice is stored as one or more **blocks** of up to 4 MiB (configurable):
  object key `data/{hash(slice_id) % 256:02x}/{slice_id}_{block_idx}_{len}`.
  The hash prefix spreads load across S3 partitions.
- Blocks are **never overwritten**. That means no S3 read-after-write
  problems, no ETag races, and a block cached anywhere is valid forever (the
  cache needs no invalidation for data, only for metadata).
- Optional per-block **compression** (lz4/zstd) and **encryption**
  (AES-256-GCM / XChaCha20-Poly1305 with keys outside the bucket), plus a
  checksum (crc32c) verified on read.
- **Compaction**: when a chunk accumulates too many slices (small random
  writes), the client or a background job reads the visible bytes, writes one
  new slice, and atomically swaps it in metad.
- **GC**: slices unreferenced after compaction/delete/truncate go to a
  delayed-delete queue in metad (default 1 h grace, longer with "trash"
  enabled) and are deleted by a GC worker. A `fsck` command cross-checks
  bucket listing against metadata to find leaks.

## 6. Metadata

### 6.1 Model

KV schema (prefix → value), stored in an ordered embedded KV store:

| Key | Value |
|---|---|
| `I{ino}` | attr: type, mode, uid, gid, size, nlink, a/m/ctime, generation |
| `D{parent_ino}{name}` | child ino + type (directory entry) |
| `C{ino}{chunk_idx}` | ordered slice list `[(slice_id, offset, len, slice_off)]` |
| `S{ino}` | symlink target |
| `X{ino}{name}` | xattr |
| `L{ino}` | lock table (fcntl ranges + flock) |
| `E{session_id}` | client session: host, mount point, heartbeat, leases held |
| `G{slice_id}` | pending-delete slices with deadline |
| `N…` | counters: next ino, next slice id range, used space/inodes |

Every FS operation (`create`, `rename`, `unlink`, `setattr`, `write commit`
etc.) is one serializable transaction, so rename is atomic, hard links and
`nlink` are exact, and directory listings are consistent.

### 6.2 Why a dedicated server (and the alternatives)

| Option | Latency per op | Push invalidation | Ops burden | Verdict |
|---|---|---|---|---|
| Metadata in S3 (log + checkpoints, conditional PUT) | 10–50 ms+ | no | none | Too slow for `ls`/`stat`; conditional writes not guaranteed on all S3-compatibles. Possible "serverless, low-concurrency" mode later. |
| External DB (Redis/Postgres/TiKV/FDB), JuiceFS-style | 0.3–2 ms | no (polling/TTL) | run a DB | Good fallback; keep as a pluggable backend. |
| **Own `rustfuse-metad`** with embedded KV | ~0.1–0.3 ms LAN; µs when cached | **yes** | one small binary | **Recommended.** Only way to get Ceph-style leases + invalidation + locks in one place. |

The server sits behind a `MetaStore` trait in the client so an external-DB
backend (e.g. Postgres or TiKV) can be added without touching the VFS.

### 6.3 Server internals

- Rust, tokio. Embedded ordered KV: **redb** or **fjall** (pure Rust, ACID)
  — pick by benchmark in Phase 1.
- RPC: request/response plus a server→client stream for lease revocations.
  Start with **tonic (gRPC)** for velocity behind an internal trait; move to a
  lean framed protocol (postcard over TCP/QUIC) if p99 latency demands it.
- Durability: local fsync'd WAL; periodic snapshot + WAL segments uploaded to
  `meta/` in the bucket, so the server can be rebuilt from the bucket alone.
- HA (Phase 5): replicate with **openraft** across 3 nodes; until then a
  restart from local disk or from the bucket backup.

## 7. Consistency model

### 7.1 Guarantees

- **Metadata is linearizable** at the server.
- **Close-to-open by default**: after client A `close()`s (or `fsync()`s) a
  file, any client that subsequently `open()`s it sees all of A's data. This
  is what NFS gives and what almost all multi-machine workflows assume.
- **Strict mode (per mount or per file via lease state)**: when only one
  client has a file open for writing it holds an *exclusive write lease* and
  may buffer freely. If a second client opens the file, the server revokes
  the lease, the holder flushes and commits, and both switch to
  *write-through*: each `write()` is uploaded and its slice committed before
  returning, so readers see it on their next `read()`. Like CephFS caps.
- **Read-your-writes** always within one client.
- **Locks**: `fcntl` (POSIX byte-range) and `flock` are enforced by metad
  across all clients. Locks belong to a session; if a client dies its
  session expires (heartbeat timeout, default 15 s) and its locks are
  released.
- **Concurrent overlapping writes** without locks: the slice committed last
  wins per byte range (deterministic order at the server). Same as local
  POSIX with two unsynchronized writers.

### 7.2 Leases and cache coherence

Clients cache attrs, directory entries and chunk maps under **leases**
granted by metad (read lease on an inode/dir; write lease on a file).

- A cached item is served locally (µs) while its lease is valid.
- Any mutation on the server first **revokes** conflicting leases (push over
  the stream), waits for ACK or lease expiry (bounded, default 2 s), then
  commits. This is what lets us cache aggressively *and* stay coherent.
- Leases are short (e.g. 30 s) and renewed by heartbeat; a partitioned client
  stops serving cached metadata once its leases expire, so a split brain
  cannot serve stale data beyond the lease window.
- The kernel's own caches are wired in: FUSE `entry_timeout`/`attr_timeout`
  follow lease state, and on revoke we call `notify_inval_inode` /
  `notify_inval_entry`. (On the NFS frontend we rely on close-to-open + short
  attribute timeouts, see §9.)

### 7.3 Durability

- `write()` returns once data is in the client's write buffer.
- `fsync()` / `close()` return once all blocks are PUT and the slice is
  committed in metad. A client crash loses only un-synced writes, exactly as a
  local disk loses un-synced page cache.
- Optional local **write-back staging** on disk (for slow/flaky links)
  is off by default because it weakens what `close()` means to other
  clients.

## 8. Performance design

### 8.1 Read path

1. Lookup chunk map (cached under lease) → list of (slice, block) ranges.
2. Serve from **memory cache** (sharded LRU, default 1 GiB) → **local disk
   cache** (NVMe directory, default 10 GiB, block files keyed by
   `slice_id/block`, needs no invalidation) → S3 ranged GET.
3. **Adaptive readahead**: detect sequential streams per file handle and
   ramp the prefetch window (e.g. 4 MiB → 256 MiB) with many parallel GETs,
   like mountpoint-s3. Random readers get exact-range GETs, no amplification.
4. Small-file prefetch: on `open()` of a file ≤ one block, fetch the whole
   block.
5. Single-flight: concurrent misses for one block share one GET.

### 8.2 Write path

1. `write()` appends to a per-handle in-memory slice buffer (bounded by a
   global memory budget with back-pressure).
2. Full blocks are uploaded immediately and in parallel; large files use
   many concurrent PUTs (not S3 multipart, since blocks are already separate
   objects, which keeps retries cheap).
3. On `fsync`/`close`/flush timer (default 5 s) the slice is committed to
   metad in one small transaction.
4. Uploaded blocks are inserted into the local cache, so read-back is free.

### 8.3 Metadata path

- Leases make repeat `stat`/`lookup`/`readdir` local.
- `readdirplus` returns attrs with entries (one RPC per directory page, not
  one per file).
- Batched/pipelined RPCs; inode and slice ID ranges pre-allocated per client.

### 8.4 S3 client

- Thin internal `ObjectStore` trait (get_range, put, delete, list, head).
- First implementation on **aws-sdk-s3** (custom endpoint, path-style
  addressing, SigV4) since it works with rustFS/MinIO/Ceph RGW/AWS. Benchmark
  against a lean `hyper` + `reqwest`-style client with our own SigV4 signing,
  and adopt it if it wins measurably on small-object latency.
- Large connection pool, request hedging for tail latency (re-issue a GET
  that exceeds p99), retries with jittered backoff, per-endpoint concurrency
  limits.

### 8.5 FUSE tuning (Linux)

`fuser` crate, multi-threaded dispatch, `max_write` 1 MiB, `max_readahead`
raised, `FUSE_WRITEBACK_CACHE` off in strict mode / on in relaxed mode,
`FUSE_PARALLEL_DIROPS`, `FUSE_DO_READDIRPLUS`, `FUSE_ASYNC_READ`,
`FOPEN_KEEP_CACHE` when the lease says the file is unchanged, splice where
supported. Track **FUSE-over-io_uring** (Linux 6.14+) as a later
optimization; it cuts per-request context-switch cost noticeably.

### 8.6 Targets (to be validated in Phase 1)

| Metric | Target (10 GbE LAN, rustFS on NVMe) |
|---|---|
| Sequential read, 1 large file | ≥ 1 GB/s per client (NIC-bound) |
| Sequential write | ≥ 800 MB/s per client |
| Cached `stat` | < 10 µs |
| Uncached `stat` / `create` | < 0.5 ms |
| `ls` of 10k-entry dir (cold) | < 100 ms |
| Small-file (4 KiB) create+write+close | > 2k ops/s per client |

## 9. Platform frontends

The VFS core exposes one async `Vfs` trait; frontends are thin adapters.

**Linux: FUSE** via `fuser` (libfuse3 not required with the pure-Rust
mount path). Default.

**macOS options**

| Option | Kernel ext? | Pros | Cons |
|---|---|---|---|
| **macFUSE ≥ 5** | kext *or* FSKit backend (macOS 26+) | `fuser` already supports it; FSKit backend avoids the kext | Third-party install; kext path needs reduced security on Apple Silicon; FSKit backend is new |
| **FUSE-T** | no (local NFSv4 server under the hood) | libfuse-compatible, kext-free | Third-party; NFS semantics leak through; closed source core |
| **NFS loopback (built-in)** | no | Zero dependencies; uses macOS's built-in NFS client; same code also serves remote/Windows clients | NFSv3 semantics: no push invalidation into the kernel (rely on short attr cache + close-to-open), no kernel-level locks passthrough (we can serve NLM later) |
| **FSKit directly** | no | Apple's official future | Swift/ObjC app-extension, signing/notarization; large effort |

**Recommendation:** ship **NFSv3 loopback** as the macOS default (Rust
`nfsserve`-style server embedded in the client, `mount_nfs` to
`localhost`), with **macFUSE** as an optional faster/fuller frontend through
the same `fuser` adapter we use on Linux. Revisit native FSKit once the core
is stable. The NFS frontend also gives a free "gateway" mode for machines
that cannot run the client.

## 10. Code layout (Cargo workspace)

```
crates/
  rustfuse-types   ids, attrs, chunk/slice math, errors, on-disk formats
  rustfuse-store   ObjectStore trait + S3 impl + test fakes
  rustfuse-meta    MetaStore trait, client (cache, leases), protocol
  rustfuse-metad   metadata server binary
  rustfuse-vfs     VFS core: read/write paths, caches, readahead, compaction
  rustfuse-fuse    FUSE frontend (Linux + macFUSE)
  rustfuse-nfs     NFSv3 loopback frontend (macOS default)
  rustfuse         CLI: format, mount, status, gc, fsck, bench, import
```

## 11. Testing and verification

- Unit + property tests (proptest) on chunk/slice merging, compaction, KV
  transactions.
- Integration tests against **rustFS and MinIO in Docker** in CI (Linux +
  macOS runners).
- POSIX conformance: **pjdfstest**, xfstests generic subset.
- Multi-client consistency: a Jepsen-style harness (multiple clients, fault
  injection: kill client, partition metad, slow S3) checking close-to-open and
  lock linearizability.
- Performance: fio, mdtest-style metadata bench, `git clone` / `cargo build`
  macro benchmarks, compared against JuiceFS and mountpoint-s3 on the same
  hardware. Benchmarks run on every release.

## 12. Phased plan

| Phase | Deliverable |
|---|---|
| **0. Foundations** | Workspace, CI (Linux/macOS), rustFS+MinIO docker harness, `ObjectStore` trait + S3 impl, bench harness. |
| **1. Single-client FS** | Data layout, embedded local metadata (same KV, in-process), Linux FUSE mount, read/write/rename/truncate/xattr, memory+disk cache, readahead. First benchmarks vs JuiceFS/mountpoint-s3. |
| **2. Multi-client** | `rustfuse-metad`, sessions, leases + invalidation, close-to-open, strict mode, fcntl/flock locks, consistency test harness. |
| **3. macOS** | NFSv3 loopback frontend; macFUSE via `fuser`; macOS CI. |
| **4. Operability** | Compaction, GC, trash, fsck, quotas, compression, encryption, metrics (Prometheus), `rustfuse status`. |
| **5. HA & scale** | openraft-replicated metad, metadata backup/restore from bucket, FUSE-over-io_uring, external-DB `MetaStore` backend if wanted. |
| **6. Interop** | Read-only passthrough mount of plain buckets; `import`/`export`. |

## 13. Open questions

1. **Bucket format** (§3): OK that the bucket holds rustFUSE blocks rather
   than plain files? (Recommended: yes.)
2. **Metadata server** (§6.2): OK to run one small `rustfuse-metad` process
   (later 3 for HA)? Where would it run in your setup (next to rustFS?)
3. **License**: Apache-2.0 / MIT dual (Rust ecosystem default) unless you
   prefer otherwise.
4. **Typical workload**: large media/datasets, many small files (source
   trees), or both? This sets default block size and cache sizes.
