use tracing_subscriber::EnvFilter;

pub fn init(filter: &str) {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(filter));

    let _ = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .try_init();
}
