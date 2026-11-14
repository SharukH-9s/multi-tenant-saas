use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Missing required configuration: {0}")]
    Missing(String),

    #[error("Invalid configuration value for {0}: {1}")]
    Validation(String, String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppConfig {
    pub app: AppSettings,
    pub database: DatabaseConfig,
    pub redis: RedisConfig,
    pub storage: StorageConfig,
    pub auth: AuthConfig,
    pub ingestion: IngestionConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppSettings {
    pub env: String,
    pub host: String,
    pub port: u16,
    pub rust_log: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseConfig {
    /// `app_runtime` account.
    pub url: String,
    /// `db_owner` account; only used by migration tooling.
    pub migration_url: Option<String>,
    pub max_connections: u32,
    pub acquire_timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedisConfig {
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageConfig {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    /// Only present when `backend` is `s3`.
    pub access_key: Option<String>,
    /// Only present when `backend` is `s3`.
    pub secret_key: Option<String>,
    /// One of `s3`, `local`, `memory`. Defaults to `s3`.
    pub backend: String,
    /// Used when `backend = local`. Defaults to `./.local-storage`.
    pub local_path: String,
    /// Whether to allow plain HTTP connections to the storage endpoint.
    /// Must be `false` in production when the endpoint is not HTTPS.
    pub allow_http: bool,
    /// Minimum multipart part size in bytes. At least 5 MiB, at most 64 MiB. Default 8 MiB.
    pub part_size_bytes: usize,
    /// Maximum upload size in bytes. Default 20 MiB, max 100 MiB.
    pub max_upload_bytes: u64,
    /// How long to retain orphaned objects before the reconciler removes them. Default 86400s.
    pub orphan_retention_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthConfig {
    pub jwt_secret: String,
    pub jwt_issuer: String,
    pub jwt_audience: String,
    pub jwt_expiry_secs: u64,
    pub refresh_token_ttl_secs: u64,
    pub argon2_memory_kib: u32,
    pub argon2_iterations: u32,
    pub argon2_parallelism: u32,
    /// Size of the semaphore bounding concurrent Argon2 hashes.
    pub max_concurrent_hashes: usize,
    pub rate_limit_window_secs: u64,
    pub rate_limit_per_ip: u32,
    pub rate_limit_per_email: u32,
    pub trusted_proxy_hops: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestionConfig {
    pub stream: String,
    pub group: String,
    pub outbox_poll_ms: u64,
    pub outbox_batch: usize,
    pub outbox_claim_secs: u64,
    pub worker_concurrency: usize,
    pub job_lease_secs: u64,
    pub job_max_attempts: u32,
    pub retry_base_secs: u64,
    pub retry_max_secs: u64,
    pub sweep_interval_secs: u64,
    pub parse_timeout_secs: u64,
    pub max_pdf_pages: u32,
    pub max_extracted_chars: usize,
    pub chunk_target_tokens: usize,
    pub chunk_overlap_tokens: usize,
    pub max_chunks_per_doc: usize,
    pub embedding_provider: String,
    pub embedding_model: String,
    pub embedding_dim: usize,
    pub embedding_batch: usize,
    pub embedding_timeout_secs: u64,
    pub embedding_api_key: Option<String>,
    pub embedding_base_url: Option<String>,
}

impl Default for IngestionConfig {
    fn default() -> Self {
        Self {
            stream: "ingestion:events".into(),
            group: "ingestion-workers".into(),
            outbox_poll_ms: 200,
            outbox_batch: 50,
            outbox_claim_secs: 30,
            worker_concurrency: 4,
            job_lease_secs: 300,
            job_max_attempts: 5,
            retry_base_secs: 5,
            retry_max_secs: 600,
            sweep_interval_secs: 15,
            parse_timeout_secs: 60,
            max_pdf_pages: 500,
            max_extracted_chars: 5_000_000,
            chunk_target_tokens: 500,
            chunk_overlap_tokens: 75,
            max_chunks_per_doc: 2000,
            embedding_provider: "fake".into(),
            embedding_model: "text-embedding-3-small".into(),
            embedding_dim: 1536,
            embedding_batch: 32,
            embedding_timeout_secs: 30,
            embedding_api_key: None,
            embedding_base_url: None,
        }
    }
}

impl IngestionConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.stream.trim().is_empty() {
            return Err(ConfigError::Validation(
                "ingestion.stream".into(),
                "must not be empty".into(),
            ));
        }
        if self.group.trim().is_empty() {
            return Err(ConfigError::Validation(
                "ingestion.group".into(),
                "must not be empty".into(),
            ));
        }
        let positive: [(&str, u64); 10] = [
            ("ingestion.outbox_poll_ms", self.outbox_poll_ms),
            ("ingestion.outbox_batch", self.outbox_batch as u64),
            ("ingestion.outbox_claim_secs", self.outbox_claim_secs),
            (
                "ingestion.worker_concurrency",
                self.worker_concurrency as u64,
            ),
            ("ingestion.job_lease_secs", self.job_lease_secs),
            ("ingestion.job_max_attempts", self.job_max_attempts as u64),
            ("ingestion.retry_base_secs", self.retry_base_secs),
            ("ingestion.sweep_interval_secs", self.sweep_interval_secs),
            ("ingestion.parse_timeout_secs", self.parse_timeout_secs),
            ("ingestion.max_pdf_pages", self.max_pdf_pages as u64),
        ];
        for (field, value) in positive {
            if value == 0 {
                return Err(ConfigError::Validation(
                    field.into(),
                    "must be greater than 0".into(),
                ));
            }
        }
        if self.max_extracted_chars == 0 {
            return Err(ConfigError::Validation(
                "ingestion.max_extracted_chars".into(),
                "must be greater than 0".into(),
            ));
        }
        if self.chunk_target_tokens == 0 {
            return Err(ConfigError::Validation(
                "ingestion.chunk_target_tokens".into(),
                "must be greater than 0".into(),
            ));
        }
        if self.chunk_overlap_tokens >= self.chunk_target_tokens {
            return Err(ConfigError::Validation(
                "ingestion.chunk_overlap_tokens".into(),
                "must be strictly less than chunk_target_tokens".into(),
            ));
        }
        if self.max_chunks_per_doc == 0 {
            return Err(ConfigError::Validation(
                "ingestion.max_chunks_per_doc".into(),
                "must be greater than 0".into(),
            ));
        }
        if self.job_lease_secs <= self.outbox_claim_secs {
            return Err(ConfigError::Validation(
                "ingestion.job_lease_secs".into(),
                "must be greater than outbox_claim_secs".into(),
            ));
        }
        if self.retry_max_secs < self.retry_base_secs {
            return Err(ConfigError::Validation(
                "ingestion.retry_max_secs".into(),
                "must be greater than or equal to retry_base_secs".into(),
            ));
        }
        match self.embedding_provider.as_str() {
            "fake" | "openai" => {}
            other => {
                return Err(ConfigError::Validation(
                    "ingestion.embedding_provider".into(),
                    format!("must be one of 'fake', 'openai'; got '{other}'"),
                ));
            }
        }
        // Without a key every embedding call would fail, so refuse to start
        // instead of letting every job burn its retries.
        if self.embedding_provider == "openai" && self.embedding_api_key.is_none() {
            return Err(ConfigError::Validation(
                "ingestion.embedding_api_key".into(),
                "EMBEDDING_API_KEY is required when EMBEDDING_PROVIDER=openai".into(),
            ));
        }
        if self.embedding_model.trim().is_empty() {
            return Err(ConfigError::Validation(
                "ingestion.embedding_model".into(),
                "must not be empty".into(),
            ));
        }
        if self.embedding_dim == 0 {
            return Err(ConfigError::Validation(
                "ingestion.embedding_dim".into(),
                "must be greater than 0".into(),
            ));
        }
        if self.embedding_dim != 1536 {
            return Err(ConfigError::Validation(
                "ingestion.embedding_dim".into(),
                "must be 1536 (fixed by pgvector column schema)".into(),
            ));
        }
        if self.embedding_batch == 0 {
            return Err(ConfigError::Validation(
                "ingestion.embedding_batch".into(),
                "must be greater than 0".into(),
            ));
        }
        if self.embedding_timeout_secs == 0 {
            return Err(ConfigError::Validation(
                "ingestion.embedding_timeout_secs".into(),
                "must be greater than 0".into(),
            ));
        }
        Ok(())
    }
}

impl AppConfig {
    /// Loads configuration from the environment (and an optional `.env` file).
    pub fn load() -> Result<Self, ConfigError> {
        let _ = dotenvy::dotenv();
        Self::load_from_env_getter(|key| std::env::var(key).ok())
    }

    /// Same as `load`, but reads values through `get_env` so tests don't touch real env vars.
    pub fn load_from_env_getter<F>(get_env: F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let required = |key: &str| get_env(key).ok_or_else(|| ConfigError::Missing(key.into()));
        let text = |key: &str, default: &str| get_env(key).unwrap_or_else(|| default.into());
        fn number<T: std::str::FromStr>(
            get_env: &impl Fn(&str) -> Option<String>,
            key: &str,
            default: T,
        ) -> Result<T, ConfigError> {
            match get_env(key) {
                None => Ok(default),
                Some(raw) => raw.parse().map_err(|_| {
                    ConfigError::Validation(key.into(), "must be a valid number".into())
                }),
            }
        }

        let config = AppConfig {
            app: AppSettings {
                env: text("APP_ENV", "development"),
                host: text("APP_HOST", "0.0.0.0"),
                port: number(&get_env, "APP_PORT", 8080)?,
                rust_log: text("RUST_LOG", "info,tenant_saas=debug"),
            },
            database: DatabaseConfig {
                url: required("DATABASE_URL")?,
                migration_url: get_env("DATABASE_MIGRATION_URL"),
                max_connections: number(&get_env, "DATABASE_MAX_CONNECTIONS", 10)?,
                acquire_timeout_secs: number(&get_env, "DATABASE_ACQUIRE_TIMEOUT_SECS", 5)?,
            },
            redis: RedisConfig {
                url: text("REDIS_URL", "redis://localhost:6379"),
            },
            storage: {
                let backend = text("OBJECT_STORAGE_BACKEND", "s3");
                let access_key = if backend == "s3" {
                    Some(required("OBJECT_STORAGE_ACCESS_KEY")?)
                } else {
                    get_env("OBJECT_STORAGE_ACCESS_KEY")
                };
                let secret_key = if backend == "s3" {
                    Some(required("OBJECT_STORAGE_SECRET_KEY")?)
                } else {
                    get_env("OBJECT_STORAGE_SECRET_KEY")
                };
                StorageConfig {
                    endpoint: text("OBJECT_STORAGE_ENDPOINT", "http://localhost:9000"),
                    bucket: text("OBJECT_STORAGE_BUCKET", "tenant-documents"),
                    region: text("OBJECT_STORAGE_REGION", "us-east-1"),
                    access_key,
                    secret_key,
                    backend,
                    local_path: text("OBJECT_STORAGE_LOCAL_PATH", "./.local-storage"),
                    allow_http: get_env("OBJECT_STORAGE_ALLOW_HTTP")
                        .map(|v| v != "false" && v != "0")
                        .unwrap_or(true),
                    part_size_bytes: number(
                        &get_env,
                        "OBJECT_STORAGE_PART_SIZE_BYTES",
                        8_388_608usize,
                    )?,
                    max_upload_bytes: number(&get_env, "MAX_UPLOAD_BYTES", 20_971_520u64)?,
                    orphan_retention_secs: number(
                        &get_env,
                        "STORAGE_ORPHAN_RETENTION_SECS",
                        86_400u64,
                    )?,
                }
            },
            auth: AuthConfig {
                jwt_secret: required("JWT_SECRET")?,
                jwt_issuer: text("JWT_ISSUER", "tenant-saas"),
                jwt_audience: text("JWT_AUDIENCE", "tenant-saas-api"),
                jwt_expiry_secs: number(&get_env, "JWT_EXPIRY_SECS", 900)?,
                refresh_token_ttl_secs: number(&get_env, "REFRESH_TOKEN_TTL_SECS", 1_209_600)?,
                argon2_memory_kib: number(&get_env, "ARGON2_MEMORY_KIB", 19_456)?,
                argon2_iterations: number(&get_env, "ARGON2_ITERATIONS", 2)?,
                argon2_parallelism: number(&get_env, "ARGON2_PARALLELISM", 1)?,
                max_concurrent_hashes: number(&get_env, "AUTH_MAX_CONCURRENT_HASHES", 4)?,
                rate_limit_window_secs: number(&get_env, "AUTH_RATE_LIMIT_WINDOW_SECS", 60)?,
                rate_limit_per_ip: number(&get_env, "AUTH_RATE_LIMIT_PER_IP", 10)?,
                rate_limit_per_email: number(&get_env, "AUTH_RATE_LIMIT_PER_EMAIL", 5)?,
                trusted_proxy_hops: number(&get_env, "AUTH_RATE_LIMIT_TRUSTED_PROXY_HOPS", 0u8)?,
            },
            ingestion: IngestionConfig {
                stream: text("INGEST_STREAM", "ingestion:events"),
                group: text("INGEST_GROUP", "ingestion-workers"),
                outbox_poll_ms: number(&get_env, "OUTBOX_POLL_MS", 200u64)?,
                outbox_batch: number(&get_env, "OUTBOX_BATCH", 50usize)?,
                outbox_claim_secs: number(&get_env, "OUTBOX_CLAIM_SECS", 30u64)?,
                worker_concurrency: number(&get_env, "WORKER_CONCURRENCY", 4usize)?,
                job_lease_secs: number(&get_env, "JOB_LEASE_SECS", 300u64)?,
                job_max_attempts: number(&get_env, "JOB_MAX_ATTEMPTS", 5u32)?,
                retry_base_secs: number(&get_env, "RETRY_BASE_SECS", 5u64)?,
                retry_max_secs: number(&get_env, "RETRY_MAX_SECS", 600u64)?,
                sweep_interval_secs: number(&get_env, "SWEEP_INTERVAL_SECS", 15u64)?,
                parse_timeout_secs: number(&get_env, "PARSE_TIMEOUT_SECS", 60u64)?,
                max_pdf_pages: number(&get_env, "MAX_PDF_PAGES", 500u32)?,
                max_extracted_chars: number(&get_env, "MAX_EXTRACTED_CHARS", 5_000_000usize)?,
                chunk_target_tokens: {
                    let key = if get_env("CHUNK_TARGET_TOKENS").is_some() {
                        "CHUNK_TARGET_TOKENS"
                    } else {
                        "CHUNK_SIZE_TOKENS"
                    };
                    number(&get_env, key, 500usize)?
                },
                chunk_overlap_tokens: number(&get_env, "CHUNK_OVERLAP_TOKENS", 75usize)?,
                max_chunks_per_doc: number(&get_env, "MAX_CHUNKS_PER_DOC", 2000usize)?,
                embedding_provider: text("EMBEDDING_PROVIDER", "fake"),
                embedding_model: text("EMBEDDING_MODEL", "text-embedding-3-small"),
                embedding_dim: {
                    let key = if get_env("EMBEDDING_DIM").is_some() {
                        "EMBEDDING_DIM"
                    } else {
                        "EMBEDDING_DIMENSION"
                    };
                    number(&get_env, key, 1536usize)?
                },
                embedding_batch: {
                    let key = if get_env("EMBEDDING_BATCH").is_some() {
                        "EMBEDDING_BATCH"
                    } else {
                        "EMBEDDING_BATCH_SIZE"
                    };
                    number(&get_env, key, 32usize)?
                },
                embedding_timeout_secs: number(&get_env, "EMBEDDING_TIMEOUT_SECS", 30u64)?,
                embedding_api_key: get_env("EMBEDDING_API_KEY").filter(|s| !s.trim().is_empty()),
                embedding_base_url: get_env("EMBEDDING_BASE_URL").filter(|s| !s.trim().is_empty()),
            },
        };

        config.validate()?;
        Ok(config)
    }

    /// Fails fast on unsafe values.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.database.url.trim().is_empty() {
            return Err(ConfigError::Validation(
                "database.url".into(),
                "must not be empty".into(),
            ));
        }
        if self.database.max_connections == 0 {
            return Err(ConfigError::Validation(
                "database.max_connections".into(),
                "must be greater than 0".into(),
            ));
        }
        if self.auth.jwt_secret.len() < 32 {
            return Err(ConfigError::Validation(
                "auth.jwt_secret".into(),
                "must be at least 32 characters long".into(),
            ));
        }
        let positive: [(&str, u64); 8] = [
            ("auth.jwt_expiry_secs", self.auth.jwt_expiry_secs),
            (
                "auth.refresh_token_ttl_secs",
                self.auth.refresh_token_ttl_secs,
            ),
            ("auth.argon2_memory_kib", self.auth.argon2_memory_kib.into()),
            ("auth.argon2_iterations", self.auth.argon2_iterations.into()),
            (
                "auth.argon2_parallelism",
                self.auth.argon2_parallelism.into(),
            ),
            (
                "auth.max_concurrent_hashes",
                self.auth.max_concurrent_hashes as u64,
            ),
            (
                "auth.rate_limit_window_secs",
                self.auth.rate_limit_window_secs,
            ),
            ("auth.rate_limit_per_ip", self.auth.rate_limit_per_ip.into()),
        ];
        for (field, value) in positive {
            if value == 0 {
                return Err(ConfigError::Validation(
                    field.into(),
                    "must be greater than 0".into(),
                ));
            }
        }
        if self.auth.rate_limit_per_email == 0 {
            return Err(ConfigError::Validation(
                "auth.rate_limit_per_email".into(),
                "must be greater than 0".into(),
            ));
        }
        if self.auth.trusted_proxy_hops > 10 {
            return Err(ConfigError::Validation(
                "auth.trusted_proxy_hops".into(),
                "must be between 0 and 10".into(),
            ));
        }
        if self.auth.jwt_issuer.trim().is_empty() || self.auth.jwt_audience.trim().is_empty() {
            return Err(ConfigError::Validation(
                "auth.jwt_issuer/jwt_audience".into(),
                "must not be empty".into(),
            ));
        }
        // --- storage ---
        match self.storage.backend.as_str() {
            "s3" | "local" | "memory" => {}
            other => {
                return Err(ConfigError::Validation(
                    "storage.backend".into(),
                    format!("must be one of s3, local, memory; got '{other}'"),
                ));
            }
        }
        if self.storage.backend == "local" && self.storage.local_path.trim().is_empty() {
            return Err(ConfigError::Validation(
                "storage.local_path".into(),
                "must not be empty when backend is local".into(),
            ));
        }
        const MIN_PART: usize = 5 * 1024 * 1024; // 5 MiB
        const MAX_PART: usize = 64 * 1024 * 1024; // 64 MiB
        if self.storage.part_size_bytes < MIN_PART || self.storage.part_size_bytes > MAX_PART {
            return Err(ConfigError::Validation(
                "storage.part_size_bytes".into(),
                "must be between 5 MiB (5242880) and 64 MiB (67108864)".into(),
            ));
        }
        if self.storage.max_upload_bytes == 0 || self.storage.max_upload_bytes > 104_857_600 {
            return Err(ConfigError::Validation(
                "storage.max_upload_bytes".into(),
                "must be between 1 and 104857600 (100 MiB)".into(),
            ));
        }
        if self.storage.orphan_retention_secs == 0 {
            return Err(ConfigError::Validation(
                "storage.orphan_retention_secs".into(),
                "must be greater than 0".into(),
            ));
        }
        self.ingestion.validate()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn mock_env() -> HashMap<&'static str, &'static str> {
        HashMap::from([
            (
                "DATABASE_URL",
                "postgres://app_runtime:runtime_password@localhost:5432/tenant_saas",
            ),
            (
                "DATABASE_MIGRATION_URL",
                "postgres://db_owner:owner_password@localhost:5432/tenant_saas",
            ),
            ("OBJECT_STORAGE_ACCESS_KEY", "minioadmin"),
            ("OBJECT_STORAGE_SECRET_KEY", "minioadmin"),
            ("JWT_SECRET", "1234567890123456789012345678901234567890"),
        ])
    }

    fn load(env: &HashMap<&str, &str>) -> Result<AppConfig, ConfigError> {
        AppConfig::load_from_env_getter(|key| env.get(key).map(|s| s.to_string()))
    }

    #[test]
    fn loads_successfully_with_standard_env_vars() {
        let cfg = load(&mock_env()).unwrap();

        assert_eq!(cfg.app.env, "development");
        assert_eq!(cfg.app.port, 8080);
        assert_eq!(
            cfg.database.url,
            "postgres://app_runtime:runtime_password@localhost:5432/tenant_saas"
        );
        assert_eq!(
            cfg.database.migration_url.as_deref(),
            Some("postgres://db_owner:owner_password@localhost:5432/tenant_saas")
        );
        assert_eq!(cfg.database.max_connections, 10);
        assert_eq!(cfg.storage.endpoint, "http://localhost:9000");
        assert_eq!(cfg.storage.backend, "s3");
        assert_eq!(cfg.storage.access_key.as_deref(), Some("minioadmin"));
        assert_eq!(cfg.storage.max_upload_bytes, 20_971_520);
        assert_eq!(cfg.storage.part_size_bytes, 8_388_608);
        assert_eq!(cfg.storage.orphan_retention_secs, 86400);
        assert_eq!(cfg.auth.jwt_expiry_secs, 900);
        assert_eq!(cfg.auth.jwt_issuer, "tenant-saas");
        assert_eq!(cfg.auth.jwt_audience, "tenant-saas-api");
        assert_eq!(cfg.auth.refresh_token_ttl_secs, 1_209_600);
        assert_eq!(cfg.auth.argon2_memory_kib, 19_456);
        assert_eq!(cfg.auth.argon2_iterations, 2);
        assert_eq!(cfg.auth.argon2_parallelism, 1);
        assert_eq!(cfg.auth.max_concurrent_hashes, 4);
        assert_eq!(cfg.auth.rate_limit_per_ip, 10);
        assert_eq!(cfg.auth.rate_limit_per_email, 5);
        assert_eq!(cfg.auth.trusted_proxy_hops, 0);

        // Ingestion defaults
        assert_eq!(cfg.ingestion.stream, "ingestion:events");
        assert_eq!(cfg.ingestion.group, "ingestion-workers");
        assert_eq!(cfg.ingestion.outbox_poll_ms, 200);
        assert_eq!(cfg.ingestion.outbox_batch, 50);
        assert_eq!(cfg.ingestion.outbox_claim_secs, 30);
        assert_eq!(cfg.ingestion.worker_concurrency, 4);
        assert_eq!(cfg.ingestion.job_lease_secs, 300);
        assert_eq!(cfg.ingestion.job_max_attempts, 5);
        assert_eq!(cfg.ingestion.retry_base_secs, 5);
        assert_eq!(cfg.ingestion.retry_max_secs, 600);
        assert_eq!(cfg.ingestion.sweep_interval_secs, 15);
        assert_eq!(cfg.ingestion.parse_timeout_secs, 60);
        assert_eq!(cfg.ingestion.max_pdf_pages, 500);
        assert_eq!(cfg.ingestion.max_extracted_chars, 5_000_000);
        assert_eq!(cfg.ingestion.chunk_target_tokens, 500);
        assert_eq!(cfg.ingestion.chunk_overlap_tokens, 75);
        assert_eq!(cfg.ingestion.max_chunks_per_doc, 2000);
        assert_eq!(cfg.ingestion.embedding_provider, "fake");
        assert_eq!(cfg.ingestion.embedding_model, "text-embedding-3-small");
        assert_eq!(cfg.ingestion.embedding_dim, 1536);
        assert_eq!(cfg.ingestion.embedding_batch, 32);
        assert_eq!(cfg.ingestion.embedding_timeout_secs, 30);
        assert_eq!(cfg.ingestion.embedding_api_key, None);
        assert_eq!(cfg.ingestion.embedding_base_url, None);
    }

    #[test]
    fn auth_overrides_are_applied() {
        let mut env = mock_env();
        env.insert("JWT_ISSUER", "other");
        env.insert("JWT_EXPIRY_SECS", "60");
        env.insert("ARGON2_ITERATIONS", "3");

        let cfg = load(&env).unwrap();

        assert_eq!(cfg.auth.jwt_issuer, "other");
        assert_eq!(cfg.auth.jwt_expiry_secs, 60);
        assert_eq!(cfg.auth.argon2_iterations, 3);
    }

    #[test]
    fn fails_when_max_concurrent_hashes_is_zero() {
        let mut env = mock_env();
        env.insert("AUTH_MAX_CONCURRENT_HASHES", "0");

        assert!(
            matches!(load(&env), Err(ConfigError::Validation(f, _)) if f == "auth.max_concurrent_hashes")
        );
    }

    #[test]
    fn custom_env_overrides_defaults() {
        let mut env = mock_env();
        env.insert("APP_PORT", "9090");
        env.insert("APP_ENV", "production");
        env.insert("DATABASE_MAX_CONNECTIONS", "25");

        let cfg = load(&env).unwrap();

        assert_eq!(cfg.app.port, 9090);
        assert_eq!(cfg.app.env, "production");
        assert_eq!(cfg.database.max_connections, 25);
    }

    #[test]
    fn fails_when_database_url_is_missing() {
        let mut env = mock_env();
        env.remove("DATABASE_URL");

        assert!(matches!(load(&env), Err(ConfigError::Missing(k)) if k == "DATABASE_URL"));
    }

    #[test]
    fn fails_when_port_is_not_a_number() {
        let mut env = mock_env();
        env.insert("APP_PORT", "abc");

        assert!(matches!(load(&env), Err(ConfigError::Validation(k, _)) if k == "APP_PORT"));
    }

    #[test]
    fn fails_when_jwt_secret_is_too_short() {
        let mut env = mock_env();
        env.insert("JWT_SECRET", "short_secret");

        assert!(matches!(load(&env), Err(ConfigError::Validation(f, _)) if f == "auth.jwt_secret"));
    }

    #[test]
    fn fails_when_max_connections_is_zero() {
        let mut env = mock_env();
        env.insert("DATABASE_MAX_CONNECTIONS", "0");

        assert!(
            matches!(load(&env), Err(ConfigError::Validation(f, _)) if f == "database.max_connections")
        );
    }

    #[test]
    fn fails_when_trusted_proxy_hops_exceeds_10() {
        let mut env = mock_env();
        env.insert("AUTH_RATE_LIMIT_TRUSTED_PROXY_HOPS", "11");
        assert!(
            matches!(load(&env), Err(ConfigError::Validation(f, _)) if f == "auth.trusted_proxy_hops")
        );
    }

    #[test]
    fn memory_backend_does_not_require_storage_keys() {
        let mut env = mock_env();
        env.remove("OBJECT_STORAGE_ACCESS_KEY");
        env.remove("OBJECT_STORAGE_SECRET_KEY");
        env.insert("OBJECT_STORAGE_BACKEND", "memory");
        let cfg = load(&env).unwrap();
        assert_eq!(cfg.storage.backend, "memory");
        assert!(cfg.storage.access_key.is_none());
    }

    #[test]
    fn invalid_storage_backend_fails_validation() {
        let mut env = mock_env();
        env.insert("OBJECT_STORAGE_BACKEND", "gcs");
        assert!(matches!(load(&env), Err(ConfigError::Validation(f, _)) if f == "storage.backend"));
    }

    #[test]
    fn part_size_below_5mib_fails_validation() {
        let mut env = mock_env();
        env.insert("OBJECT_STORAGE_PART_SIZE_BYTES", "1024");
        assert!(
            matches!(load(&env), Err(ConfigError::Validation(f, _)) if f == "storage.part_size_bytes")
        );
    }

    #[test]
    fn ingestion_overrides_are_applied() {
        let mut env = mock_env();
        env.insert("INGEST_STREAM", "custom:stream");
        env.insert("INGEST_GROUP", "custom-group");
        env.insert("OUTBOX_POLL_MS", "500");
        env.insert("OUTBOX_BATCH", "100");
        env.insert("OUTBOX_CLAIM_SECS", "45");
        env.insert("WORKER_CONCURRENCY", "8");
        env.insert("JOB_LEASE_SECS", "600");
        env.insert("JOB_MAX_ATTEMPTS", "10");
        env.insert("RETRY_BASE_SECS", "10");
        env.insert("RETRY_MAX_SECS", "1200");
        env.insert("SWEEP_INTERVAL_SECS", "30");
        env.insert("PARSE_TIMEOUT_SECS", "120");
        env.insert("MAX_PDF_PAGES", "200");
        env.insert("MAX_EXTRACTED_CHARS", "1000000");
        env.insert("CHUNK_TARGET_TOKENS", "400");
        env.insert("CHUNK_OVERLAP_TOKENS", "50");
        env.insert("MAX_CHUNKS_PER_DOC", "1000");
        env.insert("EMBEDDING_PROVIDER", "openai");
        env.insert("EMBEDDING_MODEL", "custom-embedding-model");
        env.insert("EMBEDDING_DIM", "1536");
        env.insert("EMBEDDING_BATCH", "64");
        env.insert("EMBEDDING_TIMEOUT_SECS", "45");
        env.insert("EMBEDDING_API_KEY", "sk-test-key-12345");
        env.insert("EMBEDDING_BASE_URL", "https://api.openai.com/v1");

        let cfg = load(&env).unwrap();
        assert_eq!(cfg.ingestion.stream, "custom:stream");
        assert_eq!(cfg.ingestion.group, "custom-group");
        assert_eq!(cfg.ingestion.outbox_poll_ms, 500);
        assert_eq!(cfg.ingestion.outbox_batch, 100);
        assert_eq!(cfg.ingestion.outbox_claim_secs, 45);
        assert_eq!(cfg.ingestion.worker_concurrency, 8);
        assert_eq!(cfg.ingestion.job_lease_secs, 600);
        assert_eq!(cfg.ingestion.job_max_attempts, 10);
        assert_eq!(cfg.ingestion.retry_base_secs, 10);
        assert_eq!(cfg.ingestion.retry_max_secs, 1200);
        assert_eq!(cfg.ingestion.sweep_interval_secs, 30);
        assert_eq!(cfg.ingestion.parse_timeout_secs, 120);
        assert_eq!(cfg.ingestion.max_pdf_pages, 200);
        assert_eq!(cfg.ingestion.max_extracted_chars, 1_000_000);
        assert_eq!(cfg.ingestion.chunk_target_tokens, 400);
        assert_eq!(cfg.ingestion.chunk_overlap_tokens, 50);
        assert_eq!(cfg.ingestion.max_chunks_per_doc, 1000);
        assert_eq!(cfg.ingestion.embedding_provider, "openai");
        assert_eq!(cfg.ingestion.embedding_model, "custom-embedding-model");
        assert_eq!(cfg.ingestion.embedding_dim, 1536);
        assert_eq!(cfg.ingestion.embedding_batch, 64);
        assert_eq!(cfg.ingestion.embedding_timeout_secs, 45);
        assert_eq!(
            cfg.ingestion.embedding_api_key.as_deref(),
            Some("sk-test-key-12345")
        );
        assert_eq!(
            cfg.ingestion.embedding_base_url.as_deref(),
            Some("https://api.openai.com/v1")
        );
    }

    #[test]
    fn ingestion_alias_env_vars_work() {
        let mut env = mock_env();
        env.insert("CHUNK_SIZE_TOKENS", "450");
        env.insert("EMBEDDING_DIMENSION", "1536");
        env.insert("EMBEDDING_BATCH_SIZE", "16");

        let cfg = load(&env).unwrap();
        assert_eq!(cfg.ingestion.chunk_target_tokens, 450);
        assert_eq!(cfg.ingestion.embedding_dim, 1536);
        assert_eq!(cfg.ingestion.embedding_batch, 16);
    }

    #[test]
    fn fails_when_chunk_overlap_not_less_than_target() {
        let mut env = mock_env();
        env.insert("CHUNK_TARGET_TOKENS", "500");
        env.insert("CHUNK_OVERLAP_TOKENS", "500");
        assert!(
            matches!(load(&env), Err(ConfigError::Validation(f, _)) if f == "ingestion.chunk_overlap_tokens")
        );

        let mut env2 = mock_env();
        env2.insert("CHUNK_TARGET_TOKENS", "500");
        env2.insert("CHUNK_OVERLAP_TOKENS", "600");
        assert!(
            matches!(load(&env2), Err(ConfigError::Validation(f, _)) if f == "ingestion.chunk_overlap_tokens")
        );
    }

    #[test]
    fn fails_when_embedding_dim_is_zero() {
        let mut env = mock_env();
        env.insert("EMBEDDING_DIM", "0");
        assert!(
            matches!(load(&env), Err(ConfigError::Validation(f, msg)) if f == "ingestion.embedding_dim" && msg.contains("greater than 0"))
        );
    }

    #[test]
    fn fails_when_embedding_dim_is_not_1536() {
        let mut env = mock_env();
        env.insert("EMBEDDING_DIM", "768");
        assert!(
            matches!(load(&env), Err(ConfigError::Validation(f, msg)) if f == "ingestion.embedding_dim" && msg.contains("1536"))
        );
    }

    #[test]
    fn fails_when_job_lease_not_greater_than_outbox_claim() {
        let mut env = mock_env();
        env.insert("OUTBOX_CLAIM_SECS", "30");
        env.insert("JOB_LEASE_SECS", "30");
        assert!(
            matches!(load(&env), Err(ConfigError::Validation(f, _)) if f == "ingestion.job_lease_secs")
        );

        let mut env2 = mock_env();
        env2.insert("OUTBOX_CLAIM_SECS", "50");
        env2.insert("JOB_LEASE_SECS", "30");
        assert!(
            matches!(load(&env2), Err(ConfigError::Validation(f, _)) if f == "ingestion.job_lease_secs")
        );
    }

    #[test]
    fn fails_when_embedding_provider_is_invalid() {
        let mut env = mock_env();
        env.insert("EMBEDDING_PROVIDER", "cohere");
        assert!(
            matches!(load(&env), Err(ConfigError::Validation(f, _)) if f == "ingestion.embedding_provider")
        );
    }

    #[test]
    fn fails_when_openai_provider_has_no_api_key() {
        let mut env = mock_env();
        env.insert("EMBEDDING_PROVIDER", "openai");
        assert!(
            matches!(load(&env), Err(ConfigError::Validation(f, _)) if f == "ingestion.embedding_api_key")
        );

        // A whitespace-only key is treated as missing.
        let mut env2 = mock_env();
        env2.insert("EMBEDDING_PROVIDER", "openai");
        env2.insert("EMBEDDING_API_KEY", "   ");
        assert!(
            matches!(load(&env2), Err(ConfigError::Validation(f, _)) if f == "ingestion.embedding_api_key")
        );
    }

    #[test]
    fn fake_provider_needs_no_api_key() {
        let mut env = mock_env();
        env.insert("EMBEDDING_PROVIDER", "fake");
        assert!(load(&env).is_ok());
    }

    #[test]
    fn fails_when_retry_max_less_than_retry_base() {
        let mut env = mock_env();
        env.insert("RETRY_BASE_SECS", "10");
        env.insert("RETRY_MAX_SECS", "5");
        assert!(
            matches!(load(&env), Err(ConfigError::Validation(f, _)) if f == "ingestion.retry_max_secs")
        );
    }

    #[test]
    fn fails_when_worker_concurrency_is_zero() {
        let mut env = mock_env();
        env.insert("WORKER_CONCURRENCY", "0");
        assert!(
            matches!(load(&env), Err(ConfigError::Validation(f, _)) if f == "ingestion.worker_concurrency")
        );
    }

    #[test]
    fn fails_when_ingestion_stream_or_group_is_empty() {
        let mut env = mock_env();
        env.insert("INGEST_STREAM", "   ");
        assert!(
            matches!(load(&env), Err(ConfigError::Validation(f, _)) if f == "ingestion.stream")
        );

        let mut env2 = mock_env();
        env2.insert("INGEST_GROUP", "   ");
        assert!(
            matches!(load(&env2), Err(ConfigError::Validation(f, _)) if f == "ingestion.group")
        );
    }
}
