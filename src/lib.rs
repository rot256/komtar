mod agent;
mod fifo;
mod live;
mod model;
mod server;

use std::{
    error::Error,
    io::{self, Read, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    str::FromStr,
};

use clap::{Parser, Subcommand};
use tokio::net::TcpListener;
use url::Url;

use crate::{
    agent::{AgentHub, InboxTask},
    fifo::{CommentQueue, ServerTransport},
    model::AgentMessage,
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
    after_help = "Workflow:\n  1. Run Komtar and open the printed URL.\n  2. Ask your agent to run `komtar recv`.\n  3. Right-click the page and suggest edits.\n\nRun `komtar agent` for the complete agent workflow."
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

    /// Wait for and print one batch of browser feedback.
    Recv,

    /// Send a Markdown answer to connected browsers.
    Send {
        /// CSS selector beside which the answer should appear.
        #[arg(long)]
        anchor: Option<String>,
    },

    /// Print the agent workflow.
    Agent,
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
            (None, Some(Command::Recv | Command::Send { .. } | Command::Agent)) => {
                return Err("helper commands do not start a server".into());
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
    if cli.upstream.is_some()
        && matches!(
            cli.command.as_ref(),
            Some(Command::Recv | Command::Send { .. } | Command::Agent)
        )
    {
        return Err("the legacy upstream URL cannot be combined with a helper command".into());
    }
    match &cli.command {
        Some(Command::Recv) => {
            let batch = fifo::receive_batch(&cli.fifo)?;
            io::stdout().write_all(&batch)?;
            io::stdout().flush()?;
            return Ok(());
        }
        Some(Command::Send { anchor }) => {
            let mut message = String::new();
            io::stdin().read_to_string(&mut message)?;
            let input = AgentMessage::new(message, anchor.clone())
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            agent::send_message(&cli.fifo, input)?;
            return Ok(());
        }
        Some(Command::Agent) => {
            print!("{}", agent::agent_guidance()?);
            return Ok(());
        }
        Some(Command::Proxy { .. } | Command::Serve { .. }) | None => {}
    }

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
    let transport = ServerTransport::create(&fifo_path)?;
    let transport_paths = transport.paths().clone();
    let queue = CommentQueue::new();
    let delivery = queue.start_delivery(transport_paths.clone());
    let messages = AgentHub::new();
    let inbox = InboxTask::start(transport.open_send_reader()?, messages.clone());

    let (state, mode_message, live_watcher) = match source {
        Source::Proxy(upstream) => (
            ServerState::proxy(upstream.clone(), queue, messages),
            format!("proxying {upstream}"),
            None,
        ),
        Source::Serve(directory) => {
            let excluded_paths = transport_paths.canonical().map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("could not resolve transport paths: {error}"),
                )
            })?;
            let (watcher, reload) = live::start(&directory, &excluded_paths)?;
            (
                ServerState::live(directory.clone(), excluded_paths, queue, messages, reload),
                format!("serving {} with live reload", directory.display()),
                Some(watcher),
            )
        }
    };

    let base_url = format!("http://{listen}");
    println!("komtar: {mode_message} at {base_url}");
    println!("komtar: FIFO {}", transport_paths.receive.display());
    if fifo_path == Path::new(".komtar") {
        println!("komtar: receive feedback with: komtar recv");
    } else {
        println!(
            "komtar: receive feedback with: komtar recv --fifo {}",
            fifo_path.display()
        );
    }

    let result = tokio::select! {
        result = server::serve(listener, state) => result,
        error = transport.wait_until_broken() => Err(error.into()),
    };
    drop(inbox);
    drop(delivery);
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
        assert!(help.contains("komtar recv"));
        assert!(help.contains("komtar agent"));
    }
}
