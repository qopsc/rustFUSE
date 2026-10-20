use std::ops::Range;
use std::time::Duration;
use std::time::SystemTime;

use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_config::retry::RetryConfig;
use aws_config::timeout::TimeoutConfig;
use aws_credential_types::Credentials;
use aws_sdk_s3::Client;
use aws_sdk_s3::config::http::HttpResponse;
use aws_sdk_s3::config::{Region, RequestChecksumCalculation, ResponseChecksumValidation};
use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::primitives::ByteStream;
use bytes::Bytes;

use crate::{Error, ListPage, ObjectMeta, ObjectStore, Result};

/// Connection settings for an S3-compatible endpoint.
#[derive(Clone, Debug)]
pub struct S3Config {
    pub bucket: String,
    /// Key prefix under which this filesystem lives (may be empty).
    pub prefix: String,
    /// Custom endpoint such as `http://localhost:9000`; `None` uses AWS.
    pub endpoint: Option<String>,
    pub region: String,
    /// Path-style addressing (`endpoint/bucket/key`). Needed by rustFS,
    /// MinIO and most self-hosted stores.
    pub path_style: bool,
    /// Static credentials; when `None` the standard AWS provider chain
    /// (environment, profile, instance metadata) is used.
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
    pub max_attempts: u32,
}

impl S3Config {
    pub fn new(bucket: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            prefix: String::new(),
            endpoint: None,
            region: "us-east-1".into(),
            path_style: true,
            access_key: None,
            secret_key: None,
            max_attempts: 5,
        }
    }
}

/// [`ObjectStore`] backed by the AWS SDK, usable with any S3-compatible
/// service.
#[derive(Clone, Debug)]
pub struct S3Store {
    client: Client,
    bucket: String,
    prefix: String,
}

impl S3Store {
    pub async fn connect(cfg: S3Config) -> Result<Self> {
        let mut loader = aws_config::defaults(BehaviorVersion::latest()).region(Region::new(cfg.region));
        if let (Some(ak), Some(sk)) = (cfg.access_key, cfg.secret_key) {
            loader = loader.credentials_provider(Credentials::new(ak, sk, None, None, "rustfuse"));
        }
        let shared = loader.load().await;

        let mut builder = aws_sdk_s3::config::Builder::from(&shared)
            .force_path_style(cfg.path_style)
            .retry_config(RetryConfig::standard().with_max_attempts(cfg.max_attempts))
            .timeout_config(
                TimeoutConfig::builder()
                    .connect_timeout(Duration::from_secs(5))
                    .operation_attempt_timeout(Duration::from_secs(60))
                    .build(),
            )
            // Default flexible checksums are not implemented by every
            // S3-compatible store; blocks carry their own checksum instead.
            .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
            .response_checksum_validation(ResponseChecksumValidation::WhenRequired);
        if let Some(endpoint) = cfg.endpoint {
            builder = builder.endpoint_url(endpoint);
        }

        let prefix = match cfg.prefix.trim_matches('/') {
            "" => String::new(),
            p => format!("{p}/"),
        };
        Ok(Self {
            client: Client::from_conf(builder.build()),
            bucket: cfg.bucket,
            prefix,
        })
    }

    /// Creates the bucket unless it already exists.
    pub async fn create_bucket_if_missing(&self) -> Result<()> {
        if self
            .client
            .head_bucket()
            .bucket(&self.bucket)
            .send()
            .await
            .is_ok()
        {
            return Ok(());
        }
        match self.client.create_bucket().bucket(&self.bucket).send().await {
            Ok(_) => Ok(()),
            Err(e) if matches!(e.code(), Some("BucketAlreadyOwnedByYou" | "BucketAlreadyExists")) => Ok(()),
            Err(e) => Err(Error::backend(e)),
        }
    }

    fn full_key(&self, key: &str) -> String {
        format!("{}{key}", self.prefix)
    }

    fn strip_prefix<'a>(&self, key: &'a str) -> &'a str {
        key.strip_prefix(&self.prefix).unwrap_or(key)
    }

    async fn get_inner(&self, key: &str, range: Option<String>) -> Result<Bytes> {
        let res = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(self.full_key(key))
            .set_range(range)
            .send()
            .await;
        match res {
            Ok(out) => Ok(out.body.collect().await.map_err(Error::backend)?.into_bytes()),
            Err(e) => match e.code() {
                Some("NoSuchKey") | Some("NotFound") => Err(Error::NotFound(key.to_owned())),
                // Range starts at or past the end of the object.
                Some("InvalidRange") => Ok(Bytes::new()),
                _ if http_status(&e) == Some(416) => Ok(Bytes::new()),
                _ => Err(Error::backend(e)),
            },
        }
    }
}

fn http_status<E>(e: &SdkError<E, HttpResponse>) -> Option<u16> {
    e.raw_response().map(|r| r.status().as_u16())
}

#[async_trait]
impl ObjectStore for S3Store {
    async fn get(&self, key: &str) -> Result<Bytes> {
        self.get_inner(key, None).await
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes> {
        if range.is_empty() {
            return Ok(Bytes::new());
        }
        let data = self
            .get_inner(key, Some(format!("bytes={}-{}", range.start, range.end - 1)))
            .await?;
        // Zero-length objects ignore the Range header and return 200 with an
        // empty body; anything longer than requested means the same.
        if data.len() as u64 > range.end - range.start {
            let start = range.start.min(data.len() as u64) as usize;
            let end = range.end.min(data.len() as u64) as usize;
            return Ok(data.slice(start..end));
        }
        Ok(data)
    }

    async fn put(&self, key: &str, data: Bytes) -> Result<()> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(self.full_key(key))
            .body(ByteStream::from(data))
            .send()
            .await
            .map_err(Error::backend)?;
        Ok(())
    }

    async fn put_if_absent(&self, key: &str, data: Bytes) -> Result<bool> {
        let res = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(self.full_key(key))
            .if_none_match("*")
            .body(ByteStream::from(data))
            .send()
            .await;
        match res {
            Ok(_) => Ok(true),
            // 412: the object exists. 409: another conditional write to the
            // key is in flight; the caller re-reads and decides again.
            Err(e) if matches!(http_status(&e), Some(412 | 409)) => Ok(false),
            Err(e) => Err(Error::backend(e)),
        }
    }

    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>> {
        let res = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(self.full_key(key))
            .send()
            .await;
        match res {
            Ok(out) => Ok(Some(ObjectMeta {
                key: key.to_owned(),
                size: out.content_length().unwrap_or(0).max(0) as u64,
                modified: out.last_modified().and_then(|t| SystemTime::try_from(*t).ok()),
            })),
            Err(e) if http_status(&e) == Some(404) => Ok(None),
            Err(e) => Err(Error::backend(e)),
        }
    }

    async fn delete(&self, key: &str) -> Result<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(self.full_key(key))
            .send()
            .await
            .map_err(Error::backend)?;
        Ok(())
    }

    async fn list(&self, prefix: &str, start_after: Option<&str>, limit: usize) -> Result<ListPage> {
        let out = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(self.full_key(prefix))
            .set_start_after(start_after.map(|s| self.full_key(s)))
            .max_keys(i32::try_from(limit.min(1000)).unwrap_or(1000))
            .send()
            .await
            .map_err(Error::backend)?;
        let objects: Vec<ObjectMeta> = out
            .contents()
            .iter()
            .filter_map(|o| {
                Some(ObjectMeta {
                    key: self.strip_prefix(o.key()?).to_owned(),
                    size: o.size().unwrap_or(0).max(0) as u64,
                    modified: o.last_modified().and_then(|t| SystemTime::try_from(*t).ok()),
                })
            })
            .collect();
        let next_start_after = if out.is_truncated() == Some(true) {
            objects.last().map(|o| o.key.clone())
        } else {
            None
        };
        Ok(ListPage {
            objects,
            next_start_after,
        })
    }
}
