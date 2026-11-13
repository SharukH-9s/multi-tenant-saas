//! Factory: build the concrete [`FileStorage`](crate::FileStorage) backend
//! from [`StorageConfig`].
//!
//! | `OBJECT_STORAGE_BACKEND` | Backend                 | Use case                                   |
//! |--------------------------|-------------------------|--------------------------------------------|
//! | `s3` (default)           | `AmazonS3Builder`       | MinIO locally/CI, real S3 or R2 in prod    |
//! | `local`                  | `LocalFileSystem`       | Offline development without Docker         |
//! | `memory`                 | `InMemory`              | Unit tests — no network, no disk           |

use std::sync::Arc;

use common_config::StorageConfig;
use object_store::memory::InMemory;

use crate::{object_store_backend::ObjectStoreBackend, FileStorage, StorageError};

/// Builds the [`FileStorage`] implementation selected by `config.backend`.
///
/// Returns an error only when the `s3` builder rejects the configuration
/// (e.g. a malformed endpoint URL).
pub fn build_storage(config: &StorageConfig) -> Result<Arc<dyn FileStorage>, StorageError> {
    match config.backend.as_str() {
        "s3" => build_s3(config),
        "local" => build_local(config),
        _ => Ok(Arc::new(ObjectStoreBackend::new(
            Arc::new(InMemory::new()),
            config.part_size_bytes,
        ))),
    }
}

fn build_s3(config: &StorageConfig) -> Result<Arc<dyn FileStorage>, StorageError> {
    use object_store::aws::AmazonS3Builder;

    let access_key = config.access_key.as_deref().ok_or_else(|| {
        StorageError::Backend("OBJECT_STORAGE_ACCESS_KEY is required for s3 backend".into())
    })?;
    let secret_key = config.secret_key.as_deref().ok_or_else(|| {
        StorageError::Backend("OBJECT_STORAGE_SECRET_KEY is required for s3 backend".into())
    })?;

    let store = AmazonS3Builder::new()
        .with_endpoint(&config.endpoint)
        .with_bucket_name(&config.bucket)
        .with_region(&config.region)
        .with_access_key_id(access_key)
        .with_secret_access_key(secret_key)
        .with_allow_http(config.allow_http)
        // Path-style requests are the default (virtual-hosted-style is opt-in).
        // MinIO requires path style on plain HTTP endpoints.
        .build()
        .map_err(|e| StorageError::Backend(e.to_string()))?;

    Ok(Arc::new(ObjectStoreBackend::new(
        Arc::new(store),
        config.part_size_bytes,
    )))
}

fn build_local(config: &StorageConfig) -> Result<Arc<dyn FileStorage>, StorageError> {
    use object_store::local::LocalFileSystem;
    use std::path::Path;

    let dir = Path::new(&config.local_path);
    // Create the directory tree if it doesn't exist yet.
    std::fs::create_dir_all(dir)
        .map_err(|e| StorageError::Backend(format!("cannot create local storage dir: {e}")))?;

    let store =
        LocalFileSystem::new_with_prefix(dir).map_err(|e| StorageError::Backend(e.to_string()))?;

    Ok(Arc::new(ObjectStoreBackend::new(
        Arc::new(store),
        config.part_size_bytes,
    )))
}
