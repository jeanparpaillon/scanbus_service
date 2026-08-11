//! Post-processing profile seam.
//!
//! `Job1` runs one of these once capture ends (or while pages are still arriving), with
//! profile options already resolved. Keeping this trait in core keeps the contract testable
//! without D-Bus.

use std::collections::BTreeMap;

use async_trait::async_trait;
use futures_core::stream::BoxStream;

use crate::model::{RawPage, Value};

/// Profile pipeline output before it is rendered as `Job1.Result`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileResult {
    /// One output file per page.
    Image { paths: Vec<String> },
    /// One output file for the whole scan.
    Document { path: String },
}

impl ProfileResult {
    /// Renders this profile-specific output as the `a{sv}` `Job1.Result` payload.
    pub fn to_job_result(&self) -> BTreeMap<String, Value> {
        match self {
            Self::Image { paths } => BTreeMap::from([(
                "paths".to_owned(),
                Value::Array(paths.iter().cloned().map(Value::Str).collect()),
            )]),
            Self::Document { path } => {
                BTreeMap::from([("path".to_owned(), Value::Str(path.clone()))])
            }
        }
    }
}

/// A post-processing profile implementation.
#[async_trait]
pub trait ProfileProcessor: Send + Sync {
    /// Runs post-processing on a stream of pages and resolved options.
    async fn process(
        &self,
        pages: BoxStream<'static, RawPage>,
        options: &BTreeMap<String, Value>,
    ) -> Result<ProfileResult, String>;
}
