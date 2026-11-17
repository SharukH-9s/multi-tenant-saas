//! AI provider integrations.

pub mod embedding;
pub mod fake;
pub mod openai;

use std::sync::Arc;
use std::time::Duration;

use common_config::IngestionConfig;
use common_errors::AppError;

pub use embedding::{embed_in_batches, validate_vectors, EmbeddingError, EmbeddingProvider};

/// Builds the embedder selected by `IngestionConfig` (`fake` or `openai`).
pub fn build_embedder(cfg: &IngestionConfig) -> Result<Arc<dyn EmbeddingProvider>, AppError> {
    match cfg.embedding_provider.as_str() {
        "fake" => Ok(Arc::new(fake::FakeEmbedder::new(
            cfg.embedding_model.clone(),
            cfg.embedding_dim,
        ))),
        "openai" => {
            let key = cfg
                .embedding_api_key
                .clone()
                .ok_or_else(|| AppError::Internal("EMBEDDING_API_KEY is not set".into()))?;
            let embedder = openai::OpenAiEmbedder::new(
                cfg.embedding_base_url.as_deref(),
                key,
                cfg.embedding_model.clone(),
                cfg.embedding_dim,
                Duration::from_secs(cfg.embedding_timeout_secs),
                cfg.embedding_api_version.as_deref(),
            )
            .map_err(|_| AppError::Internal("embedding client init failed".into()))?;
            Ok(Arc::new(embedder))
        }
        other => Err(AppError::Internal(format!(
            "unknown embedding provider '{other}'"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_fake_by_default() {
        let e = build_embedder(&IngestionConfig::default()).unwrap();
        assert_eq!(e.dim(), 1536);
        assert_eq!(e.model(), "text-embedding-3-small");
    }

    #[test]
    fn openai_requires_key_and_unknown_provider_fails() {
        let mut cfg = IngestionConfig {
            embedding_provider: "openai".into(),
            ..IngestionConfig::default()
        };
        assert!(build_embedder(&cfg).is_err());
        cfg.embedding_api_key = Some("k".into());
        assert!(build_embedder(&cfg).is_ok());
        cfg.embedding_provider = "nope".into();
        assert!(build_embedder(&cfg).is_err());
    }
}
