//! Owner-side manifests: completed ingests, exact object metadata and signed records.

pub(crate) mod ingest;
pub(crate) mod signing;
pub(crate) mod store;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

use crate::media_verify::KeySetBody;
use crate::upstream_cache::TtlMemo;

pub(crate) use ingest::{spawn, IngestQueue};
pub(crate) use signing::Signer;

static SIGNED_MEMO: crate::telemetry::CacheStats = crate::telemetry::CacheStats::new();

pub(crate) struct MediaState {
    pub signer: Signer,
    pub queue: Arc<IngestQueue>,
    pub keys: Vec<u8>,
    signed: TtlMemo<Vec<u8>>,
}

impl MediaState {
    pub(crate) fn from_config(config: &crate::config::Config) -> Result<Option<Arc<Self>>> {
        if !config.media_manifests {
            return Ok(None);
        }
        let signer = Signer::load(&config.media_signing_key_file)?;
        let keys = signer.sign(&KeySetBody {
            v: 1,
            issued_at: chrono::Utc::now().timestamp(),
            keys: vec![signer.public_entry()],
        })?;
        Ok(Some(Arc::new(Self {
            signer,
            queue: Arc::new(IngestQueue::default()),
            keys,
            signed: TtlMemo::new(Duration::from_secs(3600), 4096, &SIGNED_MEMO),
        })))
    }

    pub(crate) async fn signed(
        &self,
        mut manifest: crate::media_verify::ManifestBody,
    ) -> Result<Vec<u8>> {
        let key = format!("{}:{}", manifest.copy_id, manifest.revision);
        self.signed
            .get_or_try_init(&key, || async {
                manifest.kid = self.signer.kid.clone();
                self.signer.sign(&manifest)
            })
            .await
    }
}
