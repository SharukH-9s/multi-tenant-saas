use std::sync::Arc;

use common_auth::{PasswordService, PasswordSettings, TokenService, TokenSettings};
use common_config::AppConfig;
use common_errors::AppError;
use sqlx::PgPool;

use crate::services::rate_limiter::RateLimiter;

/// Shared state handed to every request handler.
#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub config: Arc<AppConfig>,
    pub token_service: Arc<TokenService>,
    pub password_service: Arc<PasswordService>,
    /// Token-bucket rate limiter backed by Redis (fail-open when Redis is unavailable).
    pub rate_limiter: Arc<RateLimiter>,
}

impl AppState {
    /// Single construction point, so callers (including tests) never build the
    /// struct literally. Fails if the Argon2 settings in the config are invalid.
    pub fn new(db: PgPool, config: Arc<AppConfig>) -> Result<Self, AppError> {
        let auth = &config.auth;

        let token_service = TokenService::new(TokenSettings {
            secret: auth.jwt_secret.clone(),
            issuer: auth.jwt_issuer.clone(),
            audience: auth.jwt_audience.clone(),
            expiry_secs: auth.jwt_expiry_secs,
        });

        let password_service = PasswordService::new(&PasswordSettings {
            memory_kib: auth.argon2_memory_kib,
            iterations: auth.argon2_iterations,
            parallelism: auth.argon2_parallelism,
            max_concurrent_hashes: auth.max_concurrent_hashes,
        })?;

        // Rate limiter connects lazily to Redis with fail-open behavior.
        let rate_limiter = RateLimiter::new(&config.redis.url, auth);

        Ok(Self {
            db,
            token_service: Arc::new(token_service),
            password_service: Arc::new(password_service),
            rate_limiter: Arc::new(rate_limiter),
            config,
        })
    }

    /// Replaces the rate limiter with another instance (e.g. for testing with custom limits).
    pub fn with_rate_limiter(mut self, limiter: RateLimiter) -> Self {
        self.rate_limiter = Arc::new(limiter);
        self
    }
}
