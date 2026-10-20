use std::collections::BTreeMap;
use std::ops::{Bound, Range};
use std::sync::Mutex;
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;

use crate::{Error, ListPage, ObjectMeta, ObjectStore, Result};

/// In-process store for tests and benchmarks of the layers above storage.
#[derive(Debug, Default)]
pub struct MemoryStore {
    objects: Mutex<BTreeMap<String, (Bytes, SystemTime)>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lookup(&self, key: &str) -> Result<Bytes> {
        self.objects
            .lock()
            .unwrap()
            .get(key)
            .map(|(d, _)| d.clone())
            .ok_or_else(|| Error::NotFound(key.to_owned()))
    }
}

#[async_trait]
impl ObjectStore for MemoryStore {
    async fn get(&self, key: &str) -> Result<Bytes> {
        self.lookup(key)
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes> {
        if range.is_empty() {
            return Ok(Bytes::new());
        }
        let data = self.lookup(key)?;
        let len = data.len() as u64;
        let start = range.start.min(len) as usize;
        let end = range.end.min(len) as usize;
        Ok(data.slice(start..end))
    }

    async fn put(&self, key: &str, data: Bytes) -> Result<()> {
        self.objects
            .lock()
            .unwrap()
            .insert(key.to_owned(), (data, SystemTime::now()));
        Ok(())
    }

    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>> {
        Ok(self.objects.lock().unwrap().get(key).map(|(d, t)| ObjectMeta {
            key: key.to_owned(),
            size: d.len() as u64,
            modified: Some(*t),
        }))
    }

    async fn delete(&self, key: &str) -> Result<()> {
        self.objects.lock().unwrap().remove(key);
        Ok(())
    }

    async fn list(&self, prefix: &str, start_after: Option<&str>, limit: usize) -> Result<ListPage> {
        let objects = self.objects.lock().unwrap();
        let lower = match start_after {
            Some(s) if s >= prefix => Bound::Excluded(s.to_owned()),
            _ => Bound::Included(prefix.to_owned()),
        };
        let mut page = ListPage::default();
        for (key, (data, t)) in objects.range((lower, Bound::Unbounded)) {
            if !key.starts_with(prefix) {
                break;
            }
            if page.objects.len() == limit {
                page.next_start_after = page.objects.last().map(|o| o.key.clone());
                break;
            }
            page.objects.push(ObjectMeta {
                key: key.clone(),
                size: data.len() as u64,
                modified: Some(*t),
            });
        }
        Ok(page)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn conformance() {
        crate::conformance::run(&MemoryStore::new(), "conformance").await;
    }
}
