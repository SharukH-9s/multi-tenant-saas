use redis::RedisError;

/// Failure talking to Redis.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    /// Connection refused or dropped, I/O error or timeout. Retry later.
    #[error("redis unavailable: {0}")]
    Unavailable(String),
    /// Anything else: wrong reply shape, server-side error.
    #[error("redis protocol error: {0}")]
    Protocol(String),
}

impl From<RedisError> for StreamError {
    fn from(error: RedisError) -> Self {
        if error.is_io_error()
            || error.is_timeout()
            || error.is_connection_refusal()
            || error.is_connection_dropped()
        {
            StreamError::Unavailable(error.to_string())
        } else {
            StreamError::Protocol(error.to_string())
        }
    }
}
