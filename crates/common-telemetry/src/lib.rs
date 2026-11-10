use tracing_subscriber::EnvFilter;

/// Initializes structured JSON logging with tracing-subscriber.
pub fn init_tracing(rust_log: &str) {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(rust_log));

    tracing_subscriber::fmt()
        .json()
        .with_env_filter(env_filter)
        .with_current_span(true)
        .init();
}
