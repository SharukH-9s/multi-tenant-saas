/// Errors returned by [`FileStorage`](crate::FileStorage) and [`ObjectWriter`](crate::ObjectWriter).
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("object not found")]
    NotFound,

    #[error("storage unavailable: {0}")]
    Unavailable(String),

    #[error("invalid object key: {0}")]
    InvalidKey(String),

    #[error("writer already finished or aborted")]
    WriterClosed,

    #[error("storage backend error: {0}")]
    Backend(String),
}
