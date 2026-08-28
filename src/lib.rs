mod fifo;
mod model;
mod server;

use std::{error::Error, net::SocketAddr, path::PathBuf, str::FromStr};

use clap::Parser;
use tokio::net::TcpListener;
use url::Url;

use crate::{
    fifo::{CommentQueue, ensure_fifo},
    server::ServerState,
};

pub(crate) type BoxError = Box<dyn Error + Send + Sync>;
pub(crate) const CLIENT_JS: &str = include_str!("../assets/client.js");

#[derive(Clone, Debug)]
struct UpstreamUrl(Url);

impl FromStr for UpstreamUrl {
    type Err = BoxError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let mut upstream = Url::parse(raw)?;
        if !matches!(upstream.scheme(), "http" | "https") {
            return Err("upstream must use http:// or https://".into());
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
        Ok(Self(upstream))
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "komtar",
    version,
    about = "Queue browser feedback as JSON records in a FIFO"
)]
pub struct Cli {
    /// Address on which komtar accepts browser requests.
    #[arg(long, default_value = "127.0.0.1:3939")]
    listen: SocketAddr,

    /// FIFO to create and use for newline-delimited JSON delivery.
    #[arg(long, default_value = ".komtar")]
    fifo: PathBuf,

    /// Base URL of the HTTP or HTTPS development server.
    upstream: UpstreamUrl,
}

pub async fn run(cli: Cli) -> Result<(), BoxError> {
    let UpstreamUrl(upstream) = cli.upstream;

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
    println!("komtar: proxying {upstream} at {base_url}");
    println!("komtar: FIFO {}", fifo.display());
    println!("komtar: read comments with: cat {}", fifo.display());

    server::serve(listener, ServerState::new(upstream, queue)).await
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, UpstreamUrl};

    #[test]
    fn parses_options_after_the_upstream() {
        let cli = Cli::try_parse_from([
            "komtar",
            "http://127.0.0.1:8000",
            "--listen",
            "127.0.0.1:0",
            "--fifo",
            "feedback.pipe",
        ])
        .expect("valid CLI");
        assert_eq!(cli.listen.port(), 0);
        assert_eq!(cli.fifo.to_string_lossy(), "feedback.pipe");
        assert_eq!(cli.upstream.0.as_str(), "http://127.0.0.1:8000/");
    }

    #[test]
    fn validates_upstream() {
        assert!("http://localhost:3000".parse::<UpstreamUrl>().is_ok());
        assert!("https://localhost:3000".parse::<UpstreamUrl>().is_ok());
        assert!("ftp://localhost:3000".parse::<UpstreamUrl>().is_err());
        assert!(
            "http://user:pass@localhost:3000"
                .parse::<UpstreamUrl>()
                .is_err()
        );
    }
}
