//! Binary entry point for the local zpay demo gateway.

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), zpay_demo::DemoError> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("zpay_demo=info,zally=info")),
        )
        .try_init();

    let config = zpay_demo::DemoConfig::from_env()?;
    zpay_demo::serve(config).await
}
