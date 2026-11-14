//! Deterministic, offline embedder for development and tests.
//!
//! **The vectors carry no semantic meaning.** The same text always gives the same
//! vector and different texts give near-orthogonal vectors, which is enough to test
//! plumbing (storage, dimensions, idempotency). Never use it to judge search relevance:
//! any relevance number measured with this embedder is meaningless.

use async_trait::async_trait;
use sha2::{Digest, Sha256};

use super::embedding::{EmbeddingError, EmbeddingProvider};

pub struct FakeEmbedder {
    model: String,
    dim: usize,
}

impl FakeEmbedder {
    pub fn new(model: impl Into<String>, dim: usize) -> Self {
        Self {
            model: model.into(),
            dim,
        }
    }

    #[allow(clippy::chunks_exact_to_as_chunks)]
    fn vector(&self, text: &str) -> Vec<f32> {
        let mut h = Sha256::new();
        h.update(self.model.as_bytes());
        h.update([0u8]);
        h.update(text.as_bytes());
        let seed = h.finalize();

        let mut v: Vec<f32> = Vec::with_capacity(self.dim);
        'outer: for i in 0..self.dim.div_ceil(8) as u32 {
            let mut h = Sha256::new();
            h.update(seed);
            h.update(i.to_le_bytes());
            for group in h.finalize().chunks_exact(4) {
                if v.len() == self.dim {
                    break 'outer;
                }
                let u = u32::from_le_bytes([group[0], group[1], group[2], group[3]]);
                v.push((f64::from(u) / f64::from(u32::MAX) * 2.0 - 1.0) as f32);
            }
        }

        let norm = v.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>().sqrt();
        if norm > 0.0 {
            for x in &mut v {
                *x = (f64::from(*x) / norm) as f32;
            }
        }
        v
    }
}

#[async_trait]
impl EmbeddingProvider for FakeEmbedder {
    fn model(&self) -> &str {
        &self.model
    }

    fn dim(&self) -> usize {
        self.dim
    }

    #[allow(clippy::chunks_exact_to_as_chunks)]
    async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        Ok(inputs.iter().map(|t| self.vector(t)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    async fn embed_one(e: &FakeEmbedder, s: &str) -> Vec<f32> {
        e.embed(&[s.to_string()]).await.unwrap().remove(0)
    }

    #[tokio::test]
    async fn same_text_gives_identical_vectors() {
        let e = FakeEmbedder::new("m", 1536);
        assert_eq!(embed_one(&e, "hello").await, embed_one(&e, "hello").await);
    }

    #[tokio::test]
    async fn different_texts_are_near_orthogonal() {
        let e = FakeEmbedder::new("m", 1536);
        let a = embed_one(&e, "hello").await;
        let b = embed_one(&e, "hello!").await;
        assert_ne!(a, b);
        assert!(cosine(&a, &b).abs() < 0.2);
    }

    #[tokio::test]
    async fn vectors_are_unit_norm_with_requested_dim() {
        for dim in [1536, 7, 8, 9] {
            let e = FakeEmbedder::new("m", dim);
            for text in ["", "a", "漢字😀", &"long ".repeat(1000)] {
                let v = embed_one(&e, text).await;
                assert_eq!(v.len(), dim);
                let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                assert!((norm - 1.0).abs() < 1e-5, "dim {dim} norm {norm}");
            }
        }
    }

    #[tokio::test]
    async fn model_name_changes_the_vector() {
        let a = embed_one(&FakeEmbedder::new("a", 64), "x").await;
        let b = embed_one(&FakeEmbedder::new("b", 64), "x").await;
        assert_ne!(a, b);
    }
}
