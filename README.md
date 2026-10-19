# rustFUSE

Mount rustFS or any S3-compatible object store as a fast, multi-client native
filesystem on Linux and macOS.

**Status:** Phase 0 (foundations). See [docs/design.md](docs/design.md) for the
architecture and roadmap.

## Layout

| Crate | Purpose |
|---|---|
| `rustfuse-types` | IDs and chunk/slice/block layout math |
| `rustfuse-store` | `ObjectStore` trait, S3 and in-memory implementations, conformance suite |
| `rustfuse` | CLI (`rustfuse bench` today; `mount`, `format`, `fsck` later) |

## Development

```sh
cargo test                                   # unit tests, in-memory store
docker compose -f dev/docker-compose.yml up -d
RUSTFUSE_TEST_S3_ENDPOINT=http://localhost:9000 cargo test   # + rustFS integration
cargo run --release -- bench --bucket rustfuse-test \
  --endpoint http://localhost:9000 --access-key rustfsadmin --secret-key rustfsadmin
```

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
