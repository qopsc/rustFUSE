//! A view of another store under a key prefix.

use std::ops::Range;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;

use crate::{ListPage, ObjectMeta, ObjectStore, Result};

/// Every key is relative to `prefix` (which ends in `/`).
pub struct PrefixStore {
    inner: Arc<dyn ObjectStore>,
    prefix: String,
}

impl PrefixStore {
    pub fn new(inner: Arc<dyn ObjectStore>, prefix: &str) -> Self {
        let p = prefix.trim_matches('/');
        PrefixStore {
            inner,
            prefix: if p.is_empty() {
                String::new()
            } else {
                format!("{p}/")
            },
        }
    }

    fn key(&self, key: &str) -> String {
        format!("{}{key}", self.prefix)
    }

    fn strip(&self, key: String) -> String {
        key.strip_prefix(&self.prefix).map(str::to_owned).unwrap_or(key)
    }
}

#[async_trait]
impl ObjectStore for PrefixStore {
    async fn get(&self, key: &str) -> Result<Bytes> {
        self.inner.get(&self.key(key)).await
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes> {
        self.inner.get_range(&self.key(key), range).await
    }

    async fn put(&self, key: &str, data: Bytes) -> Result<()> {
        self.inner.put(&self.key(key), data).await
    }

    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>> {
        Ok(self.inner.head(&self.key(key)).await?.map(|m| ObjectMeta {
            key: self.strip(m.key),
            size: m.size,
            modified: m.modified,
        }))
    }

    async fn delete(&self, key: &str) -> Result<()> {
        self.inner.delete(&self.key(key)).await
    }

    async fn list(&self, prefix: &str, start_after: Option<&str>, limit: usize) -> Result<ListPage> {
        let after = start_after.map(|a| self.key(a));
        let page = self
            .inner
            .list(&self.key(prefix), after.as_deref(), limit)
            .await?;
        Ok(ListPage {
            objects: page
                .objects
                .into_iter()
                .map(|m| ObjectMeta {
                    key: self.strip(m.key),
                    size: m.size,
                    modified: m.modified,
                })
                .collect(),
            next_start_after: page.next_start_after.map(|k| self.strip(k)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryStore;

    #[tokio::test]
    async fn conformance_and_isolation() {
        let inner: Arc<dyn ObjectStore> = Arc::new(MemoryStore::new());
        crate::conformance::run(&PrefixStore::new(inner.clone(), "volumes/a"), "t").await;
        inner.put("volumes/b/x", Bytes::from_static(b"b")).await.unwrap();
        let a = PrefixStore::new(inner.clone(), "/volumes/a/");
        a.put("x", Bytes::from_static(b"a")).await.unwrap();
        assert_eq!(inner.get("volumes/a/x").await.unwrap(), Bytes::from_static(b"a"));
        let page = a.list("", None, 100).await.unwrap();
        assert!(page.objects.iter().all(|o| !o.key.starts_with("volumes/")));
        assert!(page.objects.iter().any(|o| o.key == "x"));
    }
}
