use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Configuration loading failed: {0}")]
    Load(#[from] config::ConfigError),
}

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub app: AppSettings,
    pub database: DatabaseConfig,
    pub redis: RedisConfig,
    pub storage: StorageConfig,
    pub auth: AuthConfig,
}

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
pub struct RedisConfig {
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StorageConfig {
    pub endpoint: String,
    pub bucket: String,
    #[serde(default = "default_region")]
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
}

fn default_region() -> String {
    "us-east-1".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthConfig {
    pub jwt_secret: String,
    #[serde(default = "default_jwt_expiry_secs")]
    pub jwt_expiry_secs: u64,
}

fn default_jwt_expiry_secs() -> u64 {
    3600
}

impl AppConfig {
    pub fn load() -> Result<Self, ConfigError> {
        let _ = dotenvy::dotenv();

        let cfg = config::Config::builder()
            // Map APP_ENV, APP_HOST, APP_PORT, RUST_LOG
            .set_default("app.env", default_env())?
            .set_default("app.host", default_host())?
            .set_default("app.port", default_port() as i64)?
            .set_default("app.rust_log", default_rust_log())?
            // Map DATABASE_*
            .set_default("database.max_connections", default_max_connections() as i64)?
            .set_default(
                "database.acquire_timeout_secs",
                default_acquire_timeout_secs() as i64,
            )?
            // Storage defaults
            .set_default("storage.region", default_region())?
            // Auth defaults
            .set_default("auth.jwt_expiry_secs", default_jwt_expiry_secs() as i64)?
            // Environment variables override
            .add_source(
                config::Environment::default()
                    .separator("__")
                    .try_parsing(true),
            )
            // Also support flat standard env vars
            .add_source(
                config::Environment::default()
                    .prefix("APP")
                    .separator("_")
                    .prefix_separator("_"),
            )
            .build()?;

        let app_config: AppConfig = cfg.try_deserialize()?;
        Ok(app_config)
    }
}
