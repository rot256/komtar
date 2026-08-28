use std::{
    collections::VecDeque,
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use nix::{libc, sys::stat::Mode, unistd::mkfifo};
use tokio::sync::Notify;

use crate::model::{CommentRecord, MAX_QUEUE_RECORDS, RequestError};

const DELIVERY_INTERVAL: Duration = Duration::from_millis(150);

#[derive(Clone)]
pub(crate) struct CommentQueue {
    records: Arc<Mutex<VecDeque<CommentRecord>>>,
    notify: Arc<Notify>,
}

impl CommentQueue {
    pub(crate) fn new() -> Self {
        Self {
            records: Arc::new(Mutex::new(VecDeque::new())),
            notify: Arc::new(Notify::new()),
        }
    }

    pub(crate) fn pending(&self) -> usize {
        self.records.lock().expect("comment queue poisoned").len()
    }

    pub(crate) fn enqueue(&self, record: CommentRecord) -> Result<usize, RequestError> {
        let pending = {
            let mut records = self.records.lock().expect("comment queue poisoned");
            if records.len() >= MAX_QUEUE_RECORDS {
                return Err(RequestError::new(
                    hyper::StatusCode::INSUFFICIENT_STORAGE,
                    format!("comment queue is full ({MAX_QUEUE_RECORDS} records)"),
                ));
            }
            records.push_back(record);
            records.len()
        };
        self.notify.notify_one();
        Ok(pending)
    }

    pub(crate) fn start_delivery(&self, fifo_path: PathBuf) {
        let queue = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(DELIVERY_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = interval.tick() => {}
                    () = queue.notify.notified() => {}
                }
                if queue.pending() == 0 {
                    continue;
                }
                let queue_for_write = queue.clone();
                let path_for_write = fifo_path.clone();
                match tokio::task::spawn_blocking(move || {
                    queue_for_write.try_deliver(&path_for_write)
                })
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) if is_reader_absent(&error) => {}
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "could not deliver queued comments");
                    }
                    Err(error) => {
                        tracing::warn!(%error, "FIFO delivery task failed");
                    }
                }
            }
        });
    }

    fn try_deliver(&self, fifo_path: &Path) -> io::Result<()> {
        let mut records = self.records.lock().expect("comment queue poisoned");
        if records.is_empty() {
            return Ok(());
        }

        let mut fifo = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(fifo_path)?;

        while let Some(record) = records.front() {
            let mut bytes = serde_json::to_vec(record).map_err(io::Error::other)?;
            bytes.push(b'\n');
            write_nonblocking(&mut fifo, &bytes)?;
            records.pop_front();
        }
        Ok(())
    }
}

fn write_nonblocking(writer: &mut fs::File, bytes: &[u8]) -> io::Result<()> {
    let mut remaining = bytes;
    while !remaining.is_empty() {
        match writer.write(remaining) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "FIFO write stalled",
                ));
            }
            Ok(written) => {
                remaining = remaining.get(written..).ok_or_else(|| {
                    io::Error::other("FIFO reported an invalid number of written bytes")
                })?;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn is_reader_absent(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::WouldBlock
        || error.raw_os_error() == Some(libc::ENXIO)
        || error.raw_os_error() == Some(libc::EAGAIN)
}

pub(crate) fn ensure_fifo(path: &Path) -> io::Result<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_fifo() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "{} exists but is not a FIFO; refusing to overwrite it",
                        path.display()
                    ),
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            mkfifo(path, Mode::S_IRUSR | Mode::S_IWUSR).map_err(io::Error::other)?;
        }
        Err(error) => return Err(error),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    absolute_path(path)
}

fn absolute_path(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs, io,
        os::unix::fs::{FileTypeExt, PermissionsExt},
        thread,
        time::{Duration, Instant},
    };

    use serde::Deserialize;
    use tempfile::tempdir;

    use crate::model::{CommentRecord, PageContext, Point, PointerContext, Size, TargetContext};

    use super::{CommentQueue, ensure_fifo, is_reader_absent};

    #[derive(Deserialize)]
    struct DeliveredRecord {
        comment: String,
    }

    fn record(comment: &str) -> CommentRecord {
        CommentRecord {
            version: 1,
            id: comment.to_owned(),
            timestamp: "2026-08-29T00:00:00Z".to_owned(),
            comment: comment.to_owned(),
            page: PageContext {
                url: "http://localhost/".to_owned(),
                title: "Fixture".to_owned(),
            },
            target: TargetContext {
                selector: "p".to_owned(),
                tag: "p".to_owned(),
                id: None,
                classes: Vec::new(),
                selected_text: None,
                text: "text".to_owned(),
                html: "<p>text</p>".to_owned(),
            },
            pointer: PointerContext {
                page: Point { x: 1.0, y: 2.0 },
                viewport: Point { x: 1.0, y: 2.0 },
                target: Point { x: 1.0, y: 2.0 },
                scroll: Point { x: 0.0, y: 0.0 },
                viewport_size: Size {
                    width: 100.0,
                    height: 100.0,
                },
                target_size: Size {
                    width: 10.0,
                    height: 10.0,
                },
                device_pixel_ratio: 1.0,
            },
        }
    }

    #[test]
    fn creates_a_private_fifo() {
        let temporary = tempdir().expect("temp directory");
        let fifo = temporary.path().join("comments.fifo");
        ensure_fifo(&fifo).expect("create FIFO");
        let metadata = fs::metadata(fifo).expect("FIFO metadata");
        assert!(metadata.file_type().is_fifo());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn refuses_to_replace_a_regular_file() {
        let temporary = tempdir().expect("temp directory");
        let path = temporary.path().join("comments.fifo");
        fs::write(&path, "keep me").expect("write fixture");
        let error = ensure_fifo(&path).expect_err("regular file must be refused");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read_to_string(path).expect("fixture survives"),
            "keep me"
        );
    }

    #[test]
    fn retains_a_record_until_a_reader_arrives() {
        let temporary = tempdir().expect("temp directory");
        let fifo = temporary.path().join("comments.fifo");
        ensure_fifo(&fifo).expect("create FIFO");
        let queue = CommentQueue::new();
        queue.enqueue(record("waiting")).expect("enqueue");
        let error = queue
            .try_deliver(&fifo)
            .expect_err("no FIFO reader is present");
        assert!(is_reader_absent(&error));
        assert_eq!(queue.pending(), 1);
    }

    #[test]
    fn drains_waiting_records_in_submission_order() {
        let temporary = tempdir().expect("temp directory");
        let fifo = temporary.path().join("comments.fifo");
        ensure_fifo(&fifo).expect("create FIFO");
        let queue = CommentQueue::new();
        queue.enqueue(record("first")).expect("first record");
        queue.enqueue(record("second")).expect("second record");

        let fifo_for_reader = fifo.clone();
        let reader = thread::spawn(move || fs::read_to_string(fifo_for_reader));
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match queue.try_deliver(&fifo) {
                Ok(()) => break,
                Err(error) if is_reader_absent(&error) && Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("FIFO delivery failed: {error}"),
            }
        }
        let payload = reader.join().expect("reader thread").expect("read FIFO");
        let comments: Vec<String> = payload
            .lines()
            .map(|line| -> io::Result<String> {
                let record: DeliveredRecord =
                    serde_json::from_str(line).map_err(io::Error::other)?;
                Ok(record.comment)
            })
            .collect::<io::Result<_>>()
            .expect("JSON records");
        assert_eq!(comments, ["first", "second"]);
        assert_eq!(queue.pending(), 0);
    }
}
