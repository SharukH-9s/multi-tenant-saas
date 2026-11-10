use std::sync::Arc;

use common_config::AppConfig;
use sqlx::PgPool;

/// Shared state handed to every request handler.
#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    #[allow(dead_code)] // First read in Phase 2 (JWT settings).
    pub config: Arc<AppConfig>,
}
