use tracing_subscriber::EnvFilter;

/// Initializes structured JSON logging with tracing-subscriber.
///
/// Uses `try_init()` to safely allow multiple calls (e.g. during test suites)
/// without panicking if a global subscriber has already been installed.
pub fn init_tracing(rust_log: &str) {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(rust_log));

    let _ = tracing_subscriber::fmt()
        .json()
        .with_env_filter(env_filter)
        .with_current_span(true)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_tracing_does_not_panic() {
        init_tracing("info");
        // Calling a second time in tests must also be safe:
        init_tracing("debug");
    }
}
