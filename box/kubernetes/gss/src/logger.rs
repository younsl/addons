use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

pub fn init_tracing(level: &str, format: &str) {
    let level = match level.to_ascii_lowercase().as_str() {
        "trace" => "trace",
        "debug" => "debug",
        "warn" => "warn",
        "error" => "error",
        _ => "info",
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    let registry = tracing_subscriber::registry().with(filter);
    if format.eq_ignore_ascii_case("text") {
        registry.with(fmt::layer().with_target(true)).init();
    } else {
        registry
            .with(
                fmt::layer()
                    .json()
                    .with_current_span(true)
                    .with_span_list(false)
                    .with_target(true)
                    .with_thread_ids(true)
                    .with_thread_names(true),
            )
            .init();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_tracing() {
        // This test just ensures the logger can be initialized without panicking
        // In a real scenario, we'd test the output, but that's complex with tracing
        init_tracing("debug", "json");
    }
}
