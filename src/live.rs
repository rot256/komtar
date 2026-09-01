use std::{
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
    time::{Instant, sleep_until},
};

use crate::BoxError;

const RELOAD_DEBOUNCE: Duration = Duration::from_millis(200);
const RELOAD_MAX_DELAY: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub(crate) struct ReloadState {
    revision: watch::Sender<String>,
}

impl ReloadState {
    #[must_use]
    pub(crate) fn current(&self) -> String {
        self.revision.borrow().clone()
    }

    #[must_use]
    pub(crate) fn subscribe(&self) -> watch::Receiver<String> {
        self.revision.subscribe()
    }
}

pub(crate) struct LiveWatcher {
    _watcher: RecommendedWatcher,
    task: JoinHandle<()>,
}

impl Drop for LiveWatcher {
    fn drop(&mut self) {
        let Self { _watcher: _, task } = self;
        task.abort();
    }
}

pub(crate) fn start(root: &Path, fifo_path: &Path) -> Result<(LiveWatcher, ReloadState), BoxError> {
    let root = root.to_owned();
    let fifo_path = std::fs::canonicalize(fifo_path).unwrap_or_else(|_| fifo_path.to_owned());
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = event_tx.send(event);
    })?;
    watcher
        .watch(&root, RecursiveMode::Recursive)
        .map_err(|error| {
            io::Error::other(format!(
                "could not watch {} for changes: {error}",
                root.display()
            ))
        })?;

    let (revision, _) = watch::channel(uuid::Uuid::new_v4().to_string());
    let state = ReloadState {
        revision: revision.clone(),
    };
    let task = tokio::spawn(run_debouncer(event_rx, revision, root, fifo_path));
    Ok((
        LiveWatcher {
            _watcher: watcher,
            task,
        },
        state,
    ))
}

async fn run_debouncer(
    mut events: mpsc::UnboundedReceiver<notify::Result<Event>>,
    revision: watch::Sender<String>,
    root: PathBuf,
    fifo_path: PathBuf,
) {
    while let Some(event) = events.recv().await {
        if !should_reload(event, &root, &fifo_path) {
            continue;
        }

        let maximum_deadline = Instant::now() + RELOAD_MAX_DELAY;
        let deadline = sleep_until(Instant::now() + RELOAD_DEBOUNCE);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                biased;
                () = &mut deadline => {
                    revision.send_replace(uuid::Uuid::new_v4().to_string());
                    break;
                }
                event = events.recv() => {
                    let Some(event) = event else {
                        revision.send_replace(uuid::Uuid::new_v4().to_string());
                        tracing::warn!(
                            "filesystem watcher stopped; live reload is no longer active"
                        );
                        return;
                    };
                    if should_reload(event, &root, &fifo_path) {
                        deadline
                            .as_mut()
                            .reset((Instant::now() + RELOAD_DEBOUNCE).min(maximum_deadline));
                    }
                }
            }
        }
    }
    tracing::warn!("filesystem watcher stopped; live reload is no longer active");
}

fn should_reload(event: notify::Result<Event>, root: &Path, fifo_path: &Path) -> bool {
    let event = match event {
        Ok(event) => event,
        Err(error) => {
            tracing::warn!(%error, "filesystem watcher reported an error");
            return false;
        }
    };

    match event.kind {
        EventKind::Access(_) => false,
        EventKind::Any
        | EventKind::Create(_)
        | EventKind::Modify(_)
        | EventKind::Remove(_)
        | EventKind::Other => {
            event.paths.is_empty()
                || event.paths.iter().any(|path| {
                    let path = absolute_event_path(path, root);
                    path != fifo_path && is_visible_path(&path, root)
                })
        }
    }
}

fn is_visible_path(path: &Path, root: &Path) -> bool {
    path.strip_prefix(root).map_or(true, |relative| {
        relative
            .components()
            .all(|component| !component.as_os_str().to_string_lossy().starts_with('.'))
    })
}

fn absolute_event_path(path: &Path, root: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        root.join(path)
    }
}

#[cfg(test)]
mod tests {
    use notify::{
        Event, EventKind,
        event::{AccessKind, ModifyKind},
    };

    use super::should_reload;

    #[test]
    fn filters_fifo_hidden_and_read_events() {
        let root = std::path::Path::new("/site");
        let fifo = root.join(".komtar");
        let modify = |path| Ok(Event::new(EventKind::Modify(ModifyKind::Any)).add_path(path));

        assert!(!should_reload(modify(fifo.clone()), root, &fifo));
        assert!(!should_reload(modify(root.join(".git/index")), root, &fifo));
        assert!(should_reload(modify(root.join("index.html")), root, &fifo));
        assert!(!should_reload(
            Ok(Event::new(EventKind::Access(AccessKind::Read)).add_path(root.join("index.html"))),
            root,
            &fifo,
        ));
        assert!(should_reload(Ok(Event::new(EventKind::Any)), root, &fifo));
    }
}
