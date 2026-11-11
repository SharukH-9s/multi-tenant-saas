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
    pub access_key: String,
    pub secret_key: String,
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
            storage: StorageConfig {
                endpoint: text("OBJECT_STORAGE_ENDPOINT", "http://localhost:9000"),
                bucket: text("OBJECT_STORAGE_BUCKET", "tenant-documents"),
                region: text("OBJECT_STORAGE_REGION", "us-east-1"),
                access_key: required("OBJECT_STORAGE_ACCESS_KEY")?,
                secret_key: required("OBJECT_STORAGE_SECRET_KEY")?,
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
}
