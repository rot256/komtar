use clap::Parser;
use komtar::Cli;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "komtar=info".into()),
        )
        .with_target(false)
        .init();

    if let Err(error) = komtar::run(Cli::parse()).await {
        eprintln!("komtar: {error}");
        std::process::exit(1);
    }
}
