//! Embedding provider abstraction shared by the fake and OpenAI-compatible backends.

use std::future::Future;
use std::time::Duration;

use async_trait::async_trait;

/// `reason` strings are built by our code and never include a provider response body.
#[derive(Debug, thiserror::Error)]
pub enum EmbeddingError {
    #[error("embedding provider unavailable: {reason}")]
    Transient {
        reason: String,
        retry_after: Option<Duration>,
    },
    #[error("embedding request rejected: {reason}")]
    Permanent { reason: String },
    #[error("invalid embedding response: {reason}")]
    InvalidResponse { reason: String },
}

impl EmbeddingError {
    pub(crate) fn transient(reason: impl Into<String>) -> Self {
        Self::Transient {
            reason: reason.into(),
            retry_after: None,
        }
    }

    pub(crate) fn invalid(reason: impl Into<String>) -> Self {
        Self::InvalidResponse {
            reason: reason.into(),
        }
    }
}

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    fn model(&self) -> &str;
    fn dim(&self) -> usize;
    async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError>;
}

/// Count matches inputs, every vector has `dim` entries, every value is finite, and no
/// vector is all zeros.
pub fn validate_vectors(
    inputs: usize,
    dim: usize,
    vectors: &[Vec<f32>],
) -> Result<(), EmbeddingError> {
    if vectors.len() != inputs {
        return Err(EmbeddingError::invalid(
            "vector count does not match input count",
        ));
    }
    for v in vectors {
        if v.len() != dim {
            return Err(EmbeddingError::invalid("vector has wrong dimension"));
        }
        if v.iter().any(|x| !x.is_finite()) {
            return Err(EmbeddingError::invalid(
                "vector contains a non-finite value",
            ));
        }
        if v.iter().all(|x| *x == 0.0) {
            return Err(EmbeddingError::invalid("vector is all zeros"));
        }
    }
    Ok(())
}

/// Embeds `inputs` in sequential batches of `batch_size`, preserving order. Calls
/// `on_batch` with the cumulative number of inputs embedded after each batch (the
/// pipeline uses it to extend the job lease). `on_batch` returns `true` to continue.
/// If it returns `false`, embedding stops and `Ok(None)` is returned without further
/// provider calls.
pub async fn embed_in_batches<F, Fut>(
    p: &dyn EmbeddingProvider,
    inputs: &[String],
    batch_size: usize,
    mut on_batch: F,
) -> Result<Option<Vec<Vec<f32>>>, EmbeddingError>
where
    F: FnMut(usize) -> Fut,
    Fut: Future<Output = bool>,
{
    let mut out = Vec::with_capacity(inputs.len());
    for batch in inputs.chunks(batch_size.max(1)) {
        let vectors = p.embed(batch).await?;
        validate_vectors(batch.len(), p.dim(), &vectors)?;
        out.extend(vectors);
        if !on_batch(out.len()).await {
            return Ok(None);
        }
    }
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn validate_vectors_rejects_bad_input() {
        let ok = vec![vec![1.0, 0.0]];
        assert!(validate_vectors(1, 2, &ok).is_ok());
        assert!(validate_vectors(2, 2, &ok).is_err(), "count");
        assert!(validate_vectors(1, 3, &ok).is_err(), "dim");
        assert!(
            validate_vectors(1, 2, &[vec![f32::NAN, 1.0]]).is_err(),
            "nan"
        );
        assert!(
            validate_vectors(1, 2, &[vec![f32::INFINITY, 1.0]]).is_err(),
            "inf"
        );
        assert!(
            validate_vectors(1, 2, &[vec![f32::NEG_INFINITY, 1.0]]).is_err(),
            "-inf"
        );
        assert!(validate_vectors(1, 2, &[vec![0.0, 0.0]]).is_err(), "zero");
        assert!(validate_vectors(0, 2, &[]).is_ok());
    }

    struct Recorder {
        sizes: Mutex<Vec<usize>>,
    }

    #[async_trait]
    impl EmbeddingProvider for Recorder {
        fn model(&self) -> &str {
            "rec"
        }
        fn dim(&self) -> usize {
            2
        }
        async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
            self.sizes.lock().unwrap().push(inputs.len());
            Ok(inputs
                .iter()
                .map(|s| vec![s.parse::<f32>().unwrap() + 1.0, 1.0])
                .collect())
        }
    }

    #[tokio::test]
    async fn batches_are_sequential_ordered_and_reported() {
        let p = Recorder {
            sizes: Mutex::new(vec![]),
        };
        let inputs: Vec<String> = (0..70).map(|i| i.to_string()).collect();
        let progress = Mutex::new(Vec::new());
        let out = embed_in_batches(&p, &inputs, 32, |n| {
            progress.lock().unwrap().push(n);
            async { true }
        })
        .await
        .unwrap()
        .unwrap();

        assert_eq!(*p.sizes.lock().unwrap(), vec![32, 32, 6]);
        assert_eq!(*progress.lock().unwrap(), vec![32, 64, 70]);
        assert_eq!(out.len(), 70);
        for (i, v) in out.iter().enumerate() {
            assert_eq!(v[0], i as f32 + 1.0);
        }
    }

    #[tokio::test]
    async fn stops_early_when_batch_control_returns_false() {
        let p = Recorder {
            sizes: Mutex::new(vec![]),
        };
        let inputs: Vec<String> = (0..70).map(|i| i.to_string()).collect();
        let progress = Mutex::new(Vec::new());
        let out = embed_in_batches(&p, &inputs, 32, |n| {
            progress.lock().unwrap().push(n);
            async { false }
        })
        .await
        .unwrap();

        assert!(out.is_none());
        assert_eq!(*p.sizes.lock().unwrap(), vec![32]);
        assert_eq!(*progress.lock().unwrap(), vec![32]);
    }

    #[tokio::test]
    async fn empty_input_makes_no_calls() {
        let p = Recorder {
            sizes: Mutex::new(vec![]),
        };
        let out = embed_in_batches(&p, &[], 32, |_| async { true })
            .await
            .unwrap()
            .unwrap();
        assert!(out.is_empty());
        assert!(p.sizes.lock().unwrap().is_empty());
    }
}
