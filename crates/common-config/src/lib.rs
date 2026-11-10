use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Configuration loading failed: {0}")]
    Load(#[from] config::ConfigError),

    #[error("Missing required configuration: {0}")]
    Missing(String),

    #[error("Invalid configuration value for {0}: {1}")]
    Validation(String, String),
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    pub app: AppSettings,
    pub database: DatabaseConfig,
    pub redis: RedisConfig,
    pub storage: StorageConfig,
    pub auth: AuthConfig,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct AppSettings {
    #[serde(default = "default_env")]
    pub env: String,
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_rust_log")]
    pub rust_log: String,
}

fn default_env() -> String {
    "development".to_string()
}

fn default_host() -> String {
    "0.0.0.0".to_string()
}

fn default_port() -> u16 {
    8080
}

fn default_rust_log() -> String {
    "info,tenant_saas=debug".to_string()
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct DatabaseConfig {
    pub url: String,
    pub migration_url: Option<String>,
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
    #[serde(default = "default_acquire_timeout_secs")]
    pub acquire_timeout_secs: u64,
}

fn default_max_connections() -> u32 {
    10
}

fn default_acquire_timeout_secs() -> u64 {
    5
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct RedisConfig {
    #[serde(default = "default_redis_url")]
    pub url: String,
}

fn default_redis_url() -> String {
    "redis://localhost:6379".to_string()
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct StorageConfig {
    #[serde(default = "default_storage_endpoint")]
    pub endpoint: String,
    #[serde(default = "default_storage_bucket")]
    pub bucket: String,
    #[serde(default = "default_storage_region")]
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
}

fn default_storage_endpoint() -> String {
    "http://localhost:9000".to_string()
}

fn default_storage_bucket() -> String {
    "tenant-documents".to_string()
}

fn default_storage_region() -> String {
    "us-east-1".to_string()
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct AuthConfig {
    pub jwt_secret: String,
    #[serde(default = "default_jwt_expiry_secs")]
    pub jwt_expiry_secs: u64,
}

fn default_jwt_expiry_secs() -> u64 {
    3600
}

impl AppConfig {
    /// Loads configuration from the environment and optional `.env` file.
    ///
    /// Precedence order:
    /// 1. Explicit environment overrides (e.g. `DATABASE_URL`, `APP_PORT`)
    /// 2. Hierarchical environment overrides (e.g. `DATABASE__URL`)
    /// 3. Default fallback values
    pub fn load() -> Result<Self, ConfigError> {
        let _ = dotenvy::dotenv();
        Self::load_from_env_getter(|key| std::env::var(key).ok())
    }

    /// Internal loader accepting a key-value resolver for testability.
    pub fn load_from_env_getter<F>(get_env: F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let mut builder = config::Config::builder()
            // Safe App Defaults
            .set_default("app.env", default_env())?
            .set_default("app.host", default_host())?
            .set_default("app.port", default_port() as i64)?
            .set_default("app.rust_log", default_rust_log())?
            // Safe Database Defaults
            .set_default("database.max_connections", default_max_connections() as i64)?
            .set_default(
                "database.acquire_timeout_secs",
                default_acquire_timeout_secs() as i64,
            )?
            // Safe Redis Defaults
            .set_default("redis.url", default_redis_url())?
            // Safe Storage Defaults
            .set_default("storage.endpoint", default_storage_endpoint())?
            .set_default("storage.bucket", default_storage_bucket())?
            .set_default("storage.region", default_storage_region())?
            // Safe Auth Defaults
            .set_default("auth.jwt_expiry_secs", default_jwt_expiry_secs() as i64)?;

        // Map standard flat environment variables (matching .env.example)
        if let Some(val) = get_env("APP_ENV") {
            builder = builder.set_override("app.env", val)?;
        }
        if let Some(val) = get_env("APP_HOST") {
            builder = builder.set_override("app.host", val)?;
        }
        if let Some(val) = get_env("APP_PORT") {
            let port: i64 = val.parse().map_err(|_| {
                ConfigError::Validation("APP_PORT".into(), "must be a valid integer".into())
            })?;
            builder = builder.set_override("app.port", port)?;
        }
        if let Some(val) = get_env("RUST_LOG") {
            builder = builder.set_override("app.rust_log", val)?;
        }

        // Database overrides
        if let Some(val) = get_env("DATABASE_URL") {
            builder = builder.set_override("database.url", val)?;
        }
        if let Some(val) = get_env("DATABASE_MIGRATION_URL") {
            builder = builder.set_override("database.migration_url", val)?;
        }
        if let Some(val) = get_env("DATABASE_MAX_CONNECTIONS") {
            let max_conn: i64 = val.parse().map_err(|_| {
                ConfigError::Validation(
                    "DATABASE_MAX_CONNECTIONS".into(),
                    "must be a valid integer".into(),
                )
            })?;
            builder = builder.set_override("database.max_connections", max_conn)?;
        }
        if let Some(val) = get_env("DATABASE_ACQUIRE_TIMEOUT_SECS") {
            let timeout: i64 = val.parse().map_err(|_| {
                ConfigError::Validation(
                    "DATABASE_ACQUIRE_TIMEOUT_SECS".into(),
                    "must be a valid integer".into(),
                )
            })?;
            builder = builder.set_override("database.acquire_timeout_secs", timeout)?;
        }

        // Redis overrides
        if let Some(val) = get_env("REDIS_URL") {
            builder = builder.set_override("redis.url", val)?;
        }

        // Storage overrides
        if let Some(val) = get_env("OBJECT_STORAGE_ENDPOINT") {
            builder = builder.set_override("storage.endpoint", val)?;
        }
        if let Some(val) = get_env("OBJECT_STORAGE_BUCKET") {
            builder = builder.set_override("storage.bucket", val)?;
        }
        if let Some(val) = get_env("OBJECT_STORAGE_REGION") {
            builder = builder.set_override("storage.region", val)?;
        }
        if let Some(val) = get_env("OBJECT_STORAGE_ACCESS_KEY") {
            builder = builder.set_override("storage.access_key", val)?;
        }
        if let Some(val) = get_env("OBJECT_STORAGE_SECRET_KEY") {
            builder = builder.set_override("storage.secret_key", val)?;
        }

        // Auth overrides
        if let Some(val) = get_env("JWT_SECRET") {
            builder = builder.set_override("auth.jwt_secret", val)?;
        }
        if let Some(val) = get_env("JWT_EXPIRY_SECS") {
            let expiry: i64 = val.parse().map_err(|_| {
                ConfigError::Validation("JWT_EXPIRY_SECS".into(), "must be a valid integer".into())
            })?;
            builder = builder.set_override("auth.jwt_expiry_secs", expiry)?;
        }

        // Also support hierarchical environment variable syntax (e.g. DATABASE__URL)
        builder = builder.add_source(
            config::Environment::default()
                .separator("__")
                .try_parsing(true),
        );

        let cfg = builder.build()?;
        let app_config: AppConfig = cfg.try_deserialize()?;

        // Semantic validation: fail fast on unsafe configurations
        app_config.validate()?;

        Ok(app_config)
    }

    /// Validates safety constraints on the configuration.
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
                "must be at least 32 characters long for security".into(),
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
        let mut map = HashMap::new();
        map.insert(
            "DATABASE_URL",
            "postgres://app_runtime:runtime_password@localhost:5432/tenant_saas",
        );
        map.insert(
            "DATABASE_MIGRATION_URL",
            "postgres://db_owner:owner_password@localhost:5432/tenant_saas",
        );
        map.insert("OBJECT_STORAGE_ACCESS_KEY", "minioadmin");
        map.insert("OBJECT_STORAGE_SECRET_KEY", "minioadmin");
        map.insert(
            "JWT_SECRET",
            "1234567890123456789012345678901234567890", // >= 32 chars
        );
        map
    }

    #[test]
    fn loads_successfully_with_standard_env_vars() {
        let env = mock_env();
        let cfg =
            AppConfig::load_from_env_getter(|key| env.get(key).map(|s| s.to_string())).unwrap();

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
        assert_eq!(cfg.storage.access_key, "minioadmin");
        assert_eq!(cfg.storage.secret_key, "minioadmin");
        assert_eq!(cfg.storage.endpoint, "http://localhost:9000");
        assert_eq!(cfg.auth.jwt_expiry_secs, 3600);
    }

    #[test]
    fn custom_env_overrides_defaults() {
        let mut env = mock_env();
        env.insert("APP_PORT", "9090");
        env.insert("APP_ENV", "production");
        env.insert("DATABASE_MAX_CONNECTIONS", "25");

        let cfg =
            AppConfig::load_from_env_getter(|key| env.get(key).map(|s| s.to_string())).unwrap();

        assert_eq!(cfg.app.port, 9090);
        assert_eq!(cfg.app.env, "production");
        assert_eq!(cfg.database.max_connections, 25);
    }

    #[test]
    fn fails_when_database_url_is_missing() {
        let mut env = mock_env();
        env.remove("DATABASE_URL");

        let result = AppConfig::load_from_env_getter(|key| env.get(key).map(|s| s.to_string()));
        assert!(result.is_err());
    }

    #[test]
    fn fails_when_jwt_secret_is_too_short() {
        let mut env = mock_env();
        env.insert("JWT_SECRET", "short_secret");

        let result = AppConfig::load_from_env_getter(|key| env.get(key).map(|s| s.to_string()));
        assert!(
            matches!(result, Err(ConfigError::Validation(field, _)) if field == "auth.jwt_secret")
        );
    }

    #[test]
    fn fails_when_max_connections_is_zero() {
        let mut env = mock_env();
        env.insert("DATABASE_MAX_CONNECTIONS", "0");

        let result = AppConfig::load_from_env_getter(|key| env.get(key).map(|s| s.to_string()));
        assert!(
            matches!(result, Err(ConfigError::Validation(field, _)) if field == "database.max_connections")
        );
    }
}
