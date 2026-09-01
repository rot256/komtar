mod fifo;
mod live;
mod model;
mod server;

use std::{error::Error, io, net::SocketAddr, path::PathBuf, str::FromStr};

use clap::{Parser, Subcommand};
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
    about = "Serve or proxy development sites and queue browser feedback",
    arg_required_else_help = true,
    after_help = "Workflow:\n  1. Run Komtar and open the printed URL.\n  2. Ask your agent to read .komtar.\n  3. Right-click the page and suggest edits.\n\nThe agent reads the FIFO. You point at the page and tell it what to change."
)]
pub struct Cli {
    /// Address on which komtar accepts browser requests.
    #[arg(long, global = true, default_value = "127.0.0.1:3939")]
    listen: SocketAddr,

    /// FIFO to create and use for newline-delimited JSON delivery.
    #[arg(long, global = true, default_value = ".komtar")]
    fifo: PathBuf,

    /// Base URL of the HTTP or HTTPS development server (legacy proxy form).
    #[arg(value_name = "UPSTREAM")]
    upstream: Option<UpstreamUrl>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Proxy an HTTP or HTTPS development server.
    Proxy {
        /// Base URL of the development server.
        upstream: UpstreamUrl,
    },

    /// Serve a directory and reload browsers when its files change.
    Serve {
        /// Directory containing the site to serve.
        directory: PathBuf,
    },
}

#[derive(Debug)]
enum Source {
    Proxy(Url),
    Serve(PathBuf),
}

#[derive(Debug)]
struct RunConfig {
    listen: SocketAddr,
    fifo: PathBuf,
    source: Source,
}

impl TryFrom<Cli> for RunConfig {
    type Error = BoxError;

    fn try_from(cli: Cli) -> Result<Self, Self::Error> {
        let Cli {
            listen,
            fifo,
            upstream,
            command,
        } = cli;
        let source = match (upstream, command) {
            (Some(UpstreamUrl(upstream)), None) => Source::Proxy(upstream),
            (
                None,
                Some(Command::Proxy {
                    upstream: UpstreamUrl(upstream),
                }),
            ) => Source::Proxy(upstream),
            (None, Some(Command::Serve { directory })) => {
                let directory = std::fs::canonicalize(&directory).map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!("could not serve directory {}: {error}", directory.display()),
                    )
                })?;
                if !directory.is_dir() {
                    return Err(
                        format!("serve path {} is not a directory", directory.display()).into(),
                    );
                }
                Source::Serve(directory)
            }
            (None, None) => return Err("provide an upstream URL or a subcommand".into()),
            (Some(_), Some(_)) => {
                return Err("the legacy upstream URL cannot be combined with a subcommand".into());
            }
        };
        Ok(Self {
            listen,
            fifo,
            source,
        })
    }
}

pub async fn run(cli: Cli) -> Result<(), BoxError> {
    let RunConfig {
        listen,
        fifo: fifo_path,
        source,
    } = RunConfig::try_from(cli)?;

    let listener = TcpListener::bind(listen).await.map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!("could not listen on {listen}: {error}"),
        )
    })?;
    let listen = listener.local_addr()?;
    let fifo = ensure_fifo(&fifo_path)?;
    let queue = CommentQueue::new();
    queue.start_delivery(fifo.clone());

    let (state, mode_message, live_watcher) = match source {
        Source::Proxy(upstream) => (
            ServerState::proxy(upstream.clone(), queue),
            format!("proxying {upstream}"),
            None,
        ),
        Source::Serve(directory) => {
            let live_fifo = std::fs::canonicalize(&fifo).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("could not resolve FIFO {}: {error}", fifo.display()),
                )
            })?;
            let (watcher, reload) = live::start(&directory, &live_fifo)?;
            (
                ServerState::live(directory.clone(), live_fifo, queue, reload),
                format!("serving {} with live reload", directory.display()),
                Some(watcher),
            )
        }
    };

    let base_url = format!("http://{listen}");
    println!("komtar: {mode_message} at {base_url}");
    println!("komtar: FIFO {}", fifo.display());
    println!("komtar: read comments with: cat {}", fifo.display());

    let result = server::serve(listener, state).await;
    drop(live_watcher);
    result
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::{Cli, RunConfig, Source, UpstreamUrl};

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
        let Some(upstream) = cli.upstream else {
            panic!("legacy upstream");
        };
        assert_eq!(upstream.0.as_str(), "http://127.0.0.1:8000/");
    }

    #[test]
    fn parses_explicit_proxy_and_serve_commands() {
        let proxy = Cli::try_parse_from([
            "komtar",
            "proxy",
            "http://127.0.0.1:8000",
            "--listen",
            "127.0.0.1:0",
        ])
        .expect("explicit proxy");
        assert_eq!(proxy.listen.port(), 0);
        let proxy = RunConfig::try_from(proxy).expect("proxy run configuration");
        assert!(matches!(proxy.source, Source::Proxy(_)));

        let temporary = tempfile::tempdir().expect("temp directory");
        let serve = Cli::try_parse_from([
            "komtar",
            "--fifo",
            "feedback.pipe",
            "serve",
            temporary.path().to_str().expect("UTF-8 path"),
        ])
        .expect("serve directory");
        let config = RunConfig::try_from(serve).expect("run configuration");
        assert_eq!(config.fifo.to_string_lossy(), "feedback.pipe");
        assert!(matches!(config.source, Source::Serve(_)));
    }

    #[test]
    fn rejects_conflicting_or_missing_sources() {
        let conflicting = Cli::try_parse_from([
            "komtar",
            "http://127.0.0.1:8000",
            "proxy",
            "http://127.0.0.1:9000",
        ])
        .expect("syntactically valid CLI");
        assert!(RunConfig::try_from(conflicting).is_err());
        assert!(Cli::try_parse_from(["komtar"]).is_err());
    }

    #[test]
    fn rejects_invalid_serve_paths() {
        let temporary = tempfile::tempdir().expect("temp directory");
        let file = temporary.path().join("index.html");
        std::fs::write(&file, "fixture").expect("write fixture");

        for path in [file, temporary.path().join("missing")] {
            let cli = Cli::try_parse_from(["komtar", "serve", path.to_str().expect("UTF-8 path")])
                .expect("syntactically valid serve command");
            assert!(RunConfig::try_from(cli).is_err());
        }
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

    #[test]
    fn help_explains_the_agent_workflow() {
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("Ask your agent to read .komtar"));
        assert!(help.contains("The agent reads the FIFO"));
    }
}
