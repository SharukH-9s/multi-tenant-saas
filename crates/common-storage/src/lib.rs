//! # common-storage
//!
//! Object storage abstraction layer.
//!
//! Provides [`FileStorage`] and [`ObjectWriter`] traits backed by the
//! [`object_store`] crate, with a factory ([`build_storage`]) that selects
//! the concrete backend from [`common_config::StorageConfig`].
//!
//! | `OBJECT_STORAGE_BACKEND` | Store             | Use case                               |
//! |--------------------------|-------------------|----------------------------------------|
//! | `s3` (default)           | `AmazonS3Builder` | MinIO locally/CI, real S3/R2 in prod  |
//! | `local`                  | `LocalFileSystem` | Offline dev without Docker             |
//! | `memory`                 | `InMemory`        | Unit/integration tests                 |

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use common_config::StorageConfig;

pub mod error;
pub mod factory;
pub mod object_store_backend;

pub use error::StorageError;
pub use factory::build_storage;

// ---------------------------------------------------------------------------
// Public supporting type
// ---------------------------------------------------------------------------

/// Metadata for a stored object returned by [`FileStorage::list`].
#[derive(Debug, Clone)]
pub struct StoredObject {
    pub key: String,
    pub size_bytes: u64,
    pub last_modified: chrono::DateTime<chrono::Utc>,
}

// ---------------------------------------------------------------------------
// Core traits
// ---------------------------------------------------------------------------

/// An incremental writer for a single object.
///
/// # Contract
/// Exactly one of [`finish`](ObjectWriter::finish) or [`abort`](ObjectWriter::abort)
/// **must** be called before the writer is dropped.  S3 cannot clean up an
/// abandoned multipart upload on drop; a bucket lifecycle rule is the
/// backstop but not an excuse for omitting the explicit call.
#[async_trait]
pub trait ObjectWriter: Send {
    /// Writes the next chunk.
    async fn write(&mut self, chunk: Bytes) -> Result<(), StorageError>;

    /// Commits the object. Must be called exactly once.
    async fn finish(&mut self) -> Result<(), StorageError>;

    /// Discards everything written so far. Safe to call after a failed write.
    async fn abort(&mut self) -> Result<(), StorageError>;
}

/// Unified interface over object storage backends (S3/MinIO, local FS, in-memory).
///
/// All implementations are `Send + Sync` so they can live behind
/// `Arc<dyn FileStorage>` in `AppState`.
#[async_trait]
pub trait FileStorage: Send + Sync {
    /// Opens a writer for `key`. Nothing is visible at `key` until
    /// [`ObjectWriter::finish`] returns `Ok`.
    async fn begin_write(&self, key: &str) -> Result<Box<dyn ObjectWriter>, StorageError>;

    /// Writes an entire object in one call. Used by tests and tiny payloads.
    async fn put(&self, key: &str, bytes: Bytes) -> Result<(), StorageError>;

    /// Reads a whole object. Bounded by `MAX_UPLOAD_BYTES` in the Phase 4 worker.
    async fn get(&self, key: &str) -> Result<Bytes, StorageError>;

    /// Deletes an object. A missing key is treated as success (idempotent).
    async fn delete(&self, key: &str) -> Result<(), StorageError>;

    /// Lists objects whose keys start with `prefix`.
    async fn list(&self, prefix: &str) -> Result<Vec<StoredObject>, StorageError>;

    /// Cheap reachability probe for `/health/dependencies`. Must never write.
    async fn health(&self) -> Result<(), StorageError>;
}

// ---------------------------------------------------------------------------
// Convenience constructor for in-memory storage (tests)
// ---------------------------------------------------------------------------

/// Returns an in-memory [`FileStorage`] using the default part size (8 MiB).
/// Equivalent to calling [`build_storage`] with `backend = "memory"`.
#[cfg(test)]
pub fn in_memory_storage() -> Arc<dyn FileStorage> {
    use object_store::memory::InMemory;
    use object_store_backend::ObjectStoreBackend;
    Arc::new(ObjectStoreBackend::new(
        Arc::new(InMemory::new()),
        8 * 1024 * 1024,
    ))
}

/// Creates in-memory storage using [`StorageConfig`] defaults.
/// Used by `AppState::new` when `AppState::with_storage` is not called.
pub fn default_memory_storage(config: &StorageConfig) -> Arc<dyn FileStorage> {
    use object_store::memory::InMemory;
    use object_store_backend::ObjectStoreBackend;
    Arc::new(ObjectStoreBackend::new(
        Arc::new(InMemory::new()),
        config.part_size_bytes,
    ))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    //! All tests use the in-memory backend: no database, no network, no Docker.

    use super::*;
    use object_store::memory::InMemory;
    use object_store_backend::ObjectStoreBackend;

    const PART_SIZE: usize = 1024; // 1 KiB — small so multipart tests are cheap.

    fn storage() -> Arc<dyn FileStorage> {
        Arc::new(ObjectStoreBackend::new(
            Arc::new(InMemory::new()),
            PART_SIZE,
        ))
    }

    // -----------------------------------------------------------------------
    // put / get round trip
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn put_get_round_trip() {
        let s = storage();
        let data = Bytes::from_static(b"hello world");
        s.put("test/round-trip", data.clone()).await.unwrap();
        let got = s.get("test/round-trip").await.unwrap();
        assert_eq!(got, data);
    }

    // -----------------------------------------------------------------------
    // begin_write / finish — single-part path (payload < part_size)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn begin_write_small_chunk_uses_single_put() {
        let s = storage();
        let payload = Bytes::from(vec![b'A'; 128]); // well under 1 KiB

        let mut w = s.begin_write("test/small").await.unwrap();
        w.write(payload.clone()).await.unwrap();
        w.finish().await.unwrap();

        let got = s.get("test/small").await.unwrap();
        assert_eq!(got, payload);
    }

    #[tokio::test]
    async fn begin_write_one_byte_chunk() {
        let s = storage();
        let payload = Bytes::from_static(b"x");

        let mut w = s.begin_write("test/one-byte").await.unwrap();
        w.write(payload.clone()).await.unwrap();
        w.finish().await.unwrap();

        let got = s.get("test/one-byte").await.unwrap();
        assert_eq!(got, payload);
    }

    // -----------------------------------------------------------------------
    // begin_write / finish — multipart path (crosses two part boundaries)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn begin_write_multipart_two_boundaries() {
        let s = storage();
        // 2.5 × PART_SIZE → 3 parts total (two full + one tail).
        let total = PART_SIZE * 2 + PART_SIZE / 2;
        let payload: Bytes = (0u8..=255).cycle().take(total).collect();

        let mut w = s.begin_write("test/multipart").await.unwrap();
        // Write in chunks smaller than PART_SIZE so each write() triggers
        // a flush mid-way.
        for chunk in payload.chunks(PART_SIZE / 4) {
            w.write(Bytes::copy_from_slice(chunk)).await.unwrap();
        }
        w.finish().await.unwrap();

        let got = s.get("test/multipart").await.unwrap();
        assert_eq!(got, payload, "reassembled bytes must match the original");
    }

    // -----------------------------------------------------------------------
    // Chunk alignment: unaligned splits still produce correct output
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn unaligned_chunks_reassemble_correctly() {
        let s = storage();
        let total = PART_SIZE + 7; // crosses the boundary at an odd offset
        let payload: Bytes = (0u8..=255).cycle().take(total).collect();

        let mut w = s.begin_write("test/unaligned").await.unwrap();
        // 3-byte chunks to maximize boundary crossings.
        for chunk in payload.chunks(3) {
            w.write(Bytes::copy_from_slice(chunk)).await.unwrap();
        }
        w.finish().await.unwrap();

        let got = s.get("test/unaligned").await.unwrap();
        assert_eq!(got, payload);
    }

    // -----------------------------------------------------------------------
    // abort cleans up — get returns NotFound
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn abort_leaves_key_absent() {
        let s = storage();
        let payload = Bytes::from(vec![b'B'; PART_SIZE + 1]); // force multipart

        let mut w = s.begin_write("test/aborted").await.unwrap();
        // Write enough to open the multipart session.
        w.write(payload.clone()).await.unwrap();
        w.abort().await.unwrap();

        let result = s.get("test/aborted").await;
        assert!(
            matches!(result, Err(StorageError::NotFound)),
            "aborted write must leave no object; got {result:?}"
        );
    }

    #[tokio::test]
    async fn abort_before_first_write_leaves_key_absent() {
        let s = storage();
        let mut w = s.begin_write("test/abort-empty").await.unwrap();
        w.abort().await.unwrap();

        let result = s.get("test/abort-empty").await;
        assert!(matches!(result, Err(StorageError::NotFound)));
    }

    // -----------------------------------------------------------------------
    // Double finish returns WriterClosed
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn second_finish_returns_writer_closed() {
        let s = storage();
        let mut w = s.begin_write("test/double-finish").await.unwrap();
        w.write(Bytes::from_static(b"data")).await.unwrap();
        w.finish().await.unwrap();

        let result = w.finish().await;
        assert!(
            matches!(result, Err(StorageError::WriterClosed)),
            "second finish must return WriterClosed; got {result:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Zero-byte finish creates an empty object (handler rejects it upstream)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn zero_byte_finish_creates_empty_object() {
        let s = storage();
        let mut w = s.begin_write("test/empty").await.unwrap();
        // No write call — just finish.
        w.finish().await.unwrap();

        let got = s.get("test/empty").await.unwrap();
        assert!(
            got.is_empty(),
            "empty finish must produce a zero-byte object"
        );
    }

    // -----------------------------------------------------------------------
    // delete — missing key is treated as success (idempotent)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn delete_missing_key_is_ok() {
        let s = storage();
        // This key was never written.
        let result = s.delete("test/does-not-exist").await;
        assert!(
            result.is_ok(),
            "delete of a missing key must succeed (idempotent); got {result:?}"
        );
    }

    #[tokio::test]
    async fn delete_existing_key_removes_object() {
        let s = storage();
        s.put("test/to-delete", Bytes::from_static(b"bye"))
            .await
            .unwrap();
        s.delete("test/to-delete").await.unwrap();

        let result = s.get("test/to-delete").await;
        assert!(matches!(result, Err(StorageError::NotFound)));
    }

    // -----------------------------------------------------------------------
    // list — returns only keys under the prefix with correct sizes
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn list_returns_only_keys_under_prefix() {
        let s = storage();
        s.put("prefix-a/doc1", Bytes::from_static(b"aaa"))
            .await
            .unwrap();
        s.put("prefix-a/doc2", Bytes::from_static(b"bb"))
            .await
            .unwrap();
        s.put("prefix-b/other", Bytes::from_static(b"c"))
            .await
            .unwrap();

        let mut results = s.list("prefix-a/").await.unwrap();
        results.sort_by(|a, b| a.key.cmp(&b.key));

        assert_eq!(results.len(), 2, "must list exactly 2 keys under prefix-a/");
        assert_eq!(results[0].size_bytes, 3);
        assert_eq!(results[1].size_bytes, 2);
        // Verify no key from prefix-b leaked in.
        assert!(results.iter().all(|r| r.key.starts_with("prefix-a/")));
    }

    // -----------------------------------------------------------------------
    // InvalidKey — a key containing ".." is rejected
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn dotdot_key_is_rejected_as_invalid() {
        let s = storage();
        let result = s.put("tenants/../secrets", Bytes::from_static(b"x")).await;
        assert!(
            matches!(result, Err(StorageError::InvalidKey(_))),
            "a key with '..' must be rejected as InvalidKey; got {result:?}"
        );
    }

    #[tokio::test]
    async fn dotdot_key_begin_write_is_rejected() {
        let s = storage();
        let result = s.begin_write("tenants/../secrets").await;
        assert!(
            matches!(result, Err(StorageError::InvalidKey(_))),
            "begin_write with '..' must be rejected as InvalidKey"
        );
    }

    // -----------------------------------------------------------------------
    // health — always Ok for in-memory
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn health_returns_ok() {
        let s = storage();
        s.health().await.unwrap();
    }

    // -----------------------------------------------------------------------
    // Factory smoke test — build_storage with memory backend
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn factory_memory_backend_round_trips() {
        // Build a minimal StorageConfig with memory backend.
        let config = make_memory_config();
        let s = build_storage(&config).unwrap();
        s.put("factory/test", Bytes::from_static(b"ok"))
            .await
            .unwrap();
        let got = s.get("factory/test").await.unwrap();
        assert_eq!(got.as_ref(), b"ok");
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn make_memory_config() -> StorageConfig {
        StorageConfig {
            endpoint: "http://localhost:9000".into(),
            bucket: "test-bucket".into(),
            region: "us-east-1".into(),
            access_key: None,
            secret_key: None,
            backend: "memory".into(),
            local_path: "./.local-storage".into(),
            allow_http: true,
            part_size_bytes: PART_SIZE,
            max_upload_bytes: 20_971_520,
            orphan_retention_secs: 86400,
        }
    }
}
