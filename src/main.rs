use clap::Parser;
use web_fifo::Cli;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "web_fifo=info".into()),
        )
        .with_target(false)
        .init();

    if let Err(error) = web_fifo::run(Cli::parse()).await {
        eprintln!("web-fifo: {error}");
        std::process::exit(1);
    }
}
