//! Runs the conformance suite against a real S3-compatible endpoint.
//!
//! Skipped unless `RUSTFUSE_TEST_S3_ENDPOINT` is set, e.g. after
//! `docker compose -f dev/docker-compose.yml up -d`:
//!
//! ```text
//! RUSTFUSE_TEST_S3_ENDPOINT=http://localhost:9000 cargo test -p rustfuse-store
//! ```

use std::time::{SystemTime, UNIX_EPOCH};

use rustfuse_store::{S3Config, S3Store, conformance};

fn config(prefix: &str) -> Option<S3Config> {
    let endpoint = std::env::var("RUSTFUSE_TEST_S3_ENDPOINT").ok()?;
    let env = |k: &str, default: &str| std::env::var(k).unwrap_or_else(|_| default.to_owned());
    let mut cfg = S3Config::new(env("RUSTFUSE_TEST_S3_BUCKET", "rustfuse-test"));
    cfg.endpoint = Some(endpoint);
    cfg.prefix = prefix.to_owned();
    cfg.access_key = Some(env("RUSTFUSE_TEST_S3_ACCESS_KEY", "rustfsadmin"));
    cfg.secret_key = Some(env("RUSTFUSE_TEST_S3_SECRET_KEY", "rustfsadmin"));
    Some(cfg)
}

fn unique(name: &str) -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    format!("{name}-{nanos}")
}

async fn run(prefix: &str) {
    let Some(cfg) = config(prefix) else {
        eprintln!("RUSTFUSE_TEST_S3_ENDPOINT not set; skipping");
        return;
    };
    let store = S3Store::connect(cfg).await.unwrap();
    store.create_bucket_if_missing().await.unwrap();
    conformance::run(&store, &unique("conformance")).await;
}

#[tokio::test]
async fn s3_conformance_without_prefix() {
    run("").await;
}

#[tokio::test]
async fn s3_conformance_with_prefix() {
    run(&unique("fs")).await;
}
