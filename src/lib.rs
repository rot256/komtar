mod fifo;
mod model;
mod server;

use std::{collections::HashSet, error::Error, net::SocketAddr, path::PathBuf};

use clap::{Parser, Subcommand};
use tokio::net::TcpListener;
use url::Url;

use crate::{
    fifo::{CommentQueue, ensure_fifo},
    server::{ServerMode, ServerState},
};

pub(crate) type BoxError = Box<dyn Error + Send + Sync>;
pub(crate) const CLIENT_JS: &str = include_str!("../assets/client.js");

#[derive(Debug, Parser)]
#[command(
    name = "komtar",
    version,
    about = "Queue browser feedback as JSON records in a FIFO"
)]
pub struct Cli {
    /// Address on which komtar accepts browser requests.
    #[arg(long, global = true, default_value = "127.0.0.1:3939")]
    listen: SocketAddr,

    /// FIFO to create and use for newline-delimited JSON delivery.
    #[arg(long, global = true, default_value = ".komtar")]
    fifo: PathBuf,

    /// Additional origin allowed to use script-tag mode. May be repeated.
    #[arg(long, global = true, value_name = "URL")]
    allow_origin: Vec<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Reverse proxy a development server and inject the annotation client.
    Proxy {
        /// Base URL of the HTTP development server.
        upstream: String,
    },
    /// Serve the annotation client and API for explicit script-tag use.
    Serve,
}

pub async fn run(cli: Cli) -> Result<(), BoxError> {
    let allowed_origins = parse_allowed_origins(&cli.allow_origin)?;
    let mode = match cli.command {
        Command::Proxy { upstream } => ServerMode::Proxy {
            upstream: parse_upstream(&upstream)?,
        },
        Command::Serve => ServerMode::Serve,
    };

    let listener = TcpListener::bind(cli.listen).await.map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!("could not listen on {}: {error}", cli.listen),
        )
    })?;
    let listen = listener.local_addr()?;
    let fifo = ensure_fifo(&cli.fifo)?;
    let queue = CommentQueue::new();
    queue.start_delivery(fifo.clone());

    let base_url = format!("http://{listen}");
    match &mode {
        ServerMode::Proxy { upstream } => {
            println!("komtar: proxying {upstream} at {base_url}");
        }
        ServerMode::Serve => {
            println!("komtar: serving the annotation client at {base_url}");
        }
    }
    println!("komtar: FIFO {}", fifo.display());
    println!("komtar: read comments with: cat {}", fifo.display());
    println!(
        "komtar: script-tag fallback: <script type=\"module\" src=\"{base_url}/_komtar/client.js\"></script>"
    );

    server::serve(listener, ServerState::new(mode, queue, allowed_origins)).await
}

fn parse_upstream(raw: &str) -> Result<Url, BoxError> {
    let mut upstream = Url::parse(raw)?;
    if upstream.scheme() != "http" {
        return Err("upstream must use http://".into());
    }
    if upstream.host_str().is_none() {
        return Err("upstream must include a host".into());
    }
    if !upstream.username().is_empty() || upstream.password().is_some() {
        return Err("upstream credentials are not supported".into());
    }
    if upstream.query().is_some() || upstream.fragment().is_some() {
        return Err("upstream must not include a query or fragment".into());
    }
    if !upstream.path().ends_with('/') {
        let path = format!("{}/", upstream.path());
        upstream.set_path(&path);
    }
    Ok(upstream)
}

fn parse_allowed_origins(raw_origins: &[String]) -> Result<HashSet<String>, BoxError> {
    raw_origins
        .iter()
        .map(|raw| {
            let origin = Url::parse(raw)?;
            if !matches!(origin.scheme(), "http" | "https") || origin.host_str().is_none() {
                return Err(format!("invalid allowed origin: {raw}").into());
            }
            Ok(origin.origin().ascii_serialization())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, Command, parse_allowed_origins, parse_upstream};

    #[test]
    fn parses_proxy_options_after_the_subcommand() {
        let cli = Cli::try_parse_from([
            "komtar",
            "proxy",
            "http://127.0.0.1:8000",
            "--listen",
            "127.0.0.1:0",
            "--fifo",
            "feedback.pipe",
            "--allow-origin",
            "https://preview.example",
        ])
        .expect("valid CLI");
        assert_eq!(cli.listen.port(), 0);
        assert_eq!(cli.fifo.to_string_lossy(), "feedback.pipe");
        assert!(matches!(cli.command, Command::Proxy { .. }));
    }

    #[test]
    fn validates_upstream_and_allowed_origins() {
        assert!(parse_upstream("http://localhost:3000").is_ok());
        assert!(parse_upstream("https://localhost:3000").is_err());
        assert!(parse_upstream("http://user:pass@localhost:3000").is_err());
        let origins = parse_allowed_origins(&[
            "https://preview.example/path".to_owned(),
            "http://localhost:8080".to_owned(),
        ])
        .expect("origins");
        assert!(origins.contains("https://preview.example"));
        assert!(origins.contains("http://localhost:8080"));
    }
}
