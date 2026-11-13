//! [`ObjectStoreBackend`] and [`ObjectStoreWriter`] — the production adapter
//! over `object_store`'s `ObjectStore` trait.
//!
//! # Multipart buffering (Design Decision D6)
//!
//! Small files (≤ `part_size`) are stored with a single `put`, avoiding the
//! 5 MiB minimum part requirement and a wasted multipart session.  Larger
//! files open a multipart upload lazily on the first part flush.
//!
//! # S3 cleanup obligation
//!
//! S3 (and MinIO) cannot garbage-collect an abandoned multipart upload on
//! drop.  A bucket lifecycle rule is the backstop, but callers **must** call
//! either [`ObjectWriter::finish`](crate::ObjectWriter::finish) or
//! [`ObjectWriter::abort`](crate::ObjectWriter::abort) on every code path.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures_util::TryStreamExt;
use object_store::{path::Path, MultipartUpload, ObjectMeta, ObjectStore, ObjectStoreExt};

use crate::{FileStorage, ObjectWriter, StorageError, StoredObject};

// ---------------------------------------------------------------------------
// ObjectStoreBackend
// ---------------------------------------------------------------------------

/// [`FileStorage`] adapter backed by any `object_store::ObjectStore`.
///
/// Obtain one through [`crate::factory::build_storage`] rather than
/// constructing it directly.
pub struct ObjectStoreBackend {
    pub(crate) store: Arc<dyn ObjectStore>,
    pub(crate) part_size: usize,
}

impl ObjectStoreBackend {
    /// Wraps any `ObjectStore` implementation.
    pub fn new(store: Arc<dyn ObjectStore>, part_size: usize) -> Self {
        Self { store, part_size }
    }
}

/// Converts an `object_store::Error` into a [`StorageError`].
///
/// - `NotFound` → [`StorageError::NotFound`]
/// - `Generic` / network errors → [`StorageError::Unavailable`]
/// - everything else → [`StorageError::Backend`]
fn map_os_error(e: object_store::Error) -> StorageError {
    match e {
        object_store::Error::NotFound { .. } => StorageError::NotFound,
        object_store::Error::Generic { store: _, source } => {
            StorageError::Unavailable(source.to_string())
        }
        // Reqwest / IO errors surfaced as Generic on older versions; keep the
        // catch-all as Backend so they are still logged.
        other => StorageError::Backend(other.to_string()),
    }
}

/// Parses a string key into an `object_store::path::Path`, mapping errors to
/// [`StorageError::InvalidKey`].
fn parse_path(key: &str) -> Result<Path, StorageError> {
    // object_store rejects keys containing `..` segments.
    Path::parse(key).map_err(|e| StorageError::InvalidKey(e.to_string()))
}

#[async_trait]
impl FileStorage for ObjectStoreBackend {
    async fn begin_write(&self, key: &str) -> Result<Box<dyn ObjectWriter>, StorageError> {
        let path = parse_path(key)?;
        Ok(Box::new(ObjectStoreWriter {
            store: Arc::clone(&self.store),
            path,
            buffer: BytesMut::new(),
            part_size: self.part_size,
            upload: None,
            done: false,
        }))
    }

    async fn put(&self, key: &str, bytes: Bytes) -> Result<(), StorageError> {
        let path = parse_path(key)?;
        self.store
            .put(&path, bytes.into())
            .await
            .map(|_| ())
            .map_err(map_os_error)
    }

    async fn get(&self, key: &str) -> Result<Bytes, StorageError> {
        let path = parse_path(key)?;
        match self.store.get(&path).await {
            Ok(result) => result.bytes().await.map_err(map_os_error),
            Err(e) => Err(map_os_error(e)),
        }
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        let path = parse_path(key)?;
        match self.store.delete(&path).await {
            Ok(()) => Ok(()),
            // A missing object is treated as success: idempotent deletes
            // prevent spurious errors during compensating cleanup.
            Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(map_os_error(e)),
        }
    }

    async fn list(&self, prefix: &str) -> Result<Vec<StoredObject>, StorageError> {
        let path = parse_path(prefix)?;
        let metas: Vec<ObjectMeta> = self
            .store
            .list(Some(&path))
            .try_collect()
            .await
            .map_err(map_os_error)?;

        Ok(metas
            .into_iter()
            .map(|m| StoredObject {
                key: m.location.to_string(),
                size_bytes: m.size,
                last_modified: m.last_modified,
            })
            .collect())
    }

    /// Probes reachability by listing a fixed prefix.  Never writes, so a
    /// readiness check cannot fill the bucket.
    async fn health(&self) -> Result<(), StorageError> {
        // Any result (empty or not) means the store is reachable.
        let _ = self
            .store
            .list(Some(&Path::parse("__health__").unwrap()))
            .try_collect::<Vec<_>>()
            .await
            .map_err(map_os_error)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ObjectStoreWriter
// ---------------------------------------------------------------------------

/// Incremental writer with lazy multipart upload (see D6).
///
/// # Contract
///
/// Exactly **one** of [`finish`](ObjectStoreWriter::finish) or
/// [`abort`](ObjectStoreWriter::abort) must be called before the writer is
/// dropped.  S3 cannot clean up an abandoned multipart upload on drop; a
/// bucket lifecycle rule provides a backstop but is not a substitute for the
/// explicit call.
pub struct ObjectStoreWriter {
    store: Arc<dyn ObjectStore>,
    path: Path,
    buffer: BytesMut,
    part_size: usize,
    /// `None` until the first part flush.  Once opened, multipart is used
    /// for all subsequent parts and the final commit.
    upload: Option<Box<dyn MultipartUpload>>,
    done: bool,
}

#[async_trait]
impl ObjectWriter for ObjectStoreWriter {
    /// Appends `chunk` to the internal buffer and flushes complete parts.
    ///
    /// A multipart session is opened lazily on the first flush.
    async fn write(&mut self, chunk: Bytes) -> Result<(), StorageError> {
        if self.done {
            return Err(StorageError::WriterClosed);
        }

        self.buffer.extend_from_slice(&chunk);

        // Flush all complete parts.
        while self.buffer.len() >= self.part_size {
            let part = self.buffer.split_to(self.part_size).freeze();
            self.flush_part(part).await?;
        }

        Ok(())
    }

    /// Commits the object.
    ///
    /// - If no multipart session was opened (payload fits in one part), a
    ///   single `put` avoids the 5 MiB S3 floor.
    /// - Otherwise the remaining buffer is sent as the final part and the
    ///   multipart upload is completed.
    ///
    /// Returns [`StorageError::WriterClosed`] if called more than once.
    async fn finish(&mut self) -> Result<(), StorageError> {
        if self.done {
            return Err(StorageError::WriterClosed);
        }
        self.done = true;

        match self.upload.take() {
            None => {
                // Single-part path: everything fits in the buffer.
                let data = self.buffer.split().freeze();
                self.store
                    .put(&self.path, data.into())
                    .await
                    .map(|_| ())
                    .map_err(map_os_error)
            }
            Some(mut upload) => {
                // Send the remaining buffer as the final (possibly small) part.
                if !self.buffer.is_empty() {
                    let tail = self.buffer.split().freeze();
                    upload.put_part(tail.into()).await.map_err(map_os_error)?;
                }
                upload.complete().await.map(|_| ()).map_err(map_os_error)
            }
        }
    }

    /// Discards all buffered data and aborts the multipart upload if one was
    /// opened.  Safe to call after a failed [`write`](ObjectStoreWriter::write)
    /// or on any error exit path.  Idempotent after the first call.
    async fn abort(&mut self) -> Result<(), StorageError> {
        self.buffer.clear();
        self.done = true;
        if let Some(mut upload) = self.upload.take() {
            upload.abort().await.map_err(map_os_error)?;
        }
        Ok(())
    }
}

impl ObjectStoreWriter {
    /// Sends `part` as the next multipart part, opening the session on first
    /// call.
    async fn flush_part(&mut self, part: Bytes) -> Result<(), StorageError> {
        let upload = match self.upload {
            Some(ref mut u) => u,
            None => {
                let new_upload = self
                    .store
                    .put_multipart(&self.path)
                    .await
                    .map_err(map_os_error)?;
                self.upload = Some(new_upload);
                self.upload.as_mut().unwrap()
            }
        };
        upload.put_part(part.into()).await.map_err(map_os_error)
    }
}
