//! Remote storage abstraction. Only Google Cloud Storage is implemented today;
//! another provider only needs an [`Uploader`] impl and a [`Provider`] variant.

use std::path::Path;

use crate::config::{Provider, UploadConfig};

pub struct Uploaded {
    pub bytes: u64,
    /// The object was already there with the same size (a retry after a crash).
    pub existed: bool,
}

pub trait Uploader: Send + Sync {
    /// Uploads `path` as `object`, never overwriting an existing object.
    fn upload(&self, path: &Path, object: &str, content_type: &str) -> Result<Uploaded, String>;
    /// Display URL of an object (`gs://bucket/object`).
    fn url(&self, object: &str) -> String;
    /// Verifies credentials and bucket access; returns a human-readable summary.
    fn check(&self) -> Result<String, String>;
}

pub fn from_config(cfg: &UploadConfig) -> Result<Box<dyn Uploader>, String> {
    match cfg.provider {
        #[cfg(feature = "gcs")]
        Provider::Gcs => Ok(Box::new(crate::gcs::Gcs::new(cfg)?)),
        #[cfg(not(feature = "gcs"))]
        Provider::Gcs => Err("rotlog was built without the `gcs` feature".into()),
    }
}
