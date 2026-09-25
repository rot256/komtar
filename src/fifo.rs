use std::{
    collections::VecDeque,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use nix::{
    fcntl::AT_FDCWD,
    libc,
    sys::stat::{FchmodatFlags, Mode, fchmodat},
    unistd::mkfifo,
};
use tokio::{sync::Notify, task::JoinHandle};

use crate::model::{CommentRecord, MAX_QUEUE_RECORDS, RequestError};

const DELIVERY_INTERVAL: Duration = Duration::from_millis(150);
const RECEIVE_POLL_INTERVAL: Duration = Duration::from_millis(10);
const TRANSPORT_CHECK_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone, Debug)]
pub(crate) struct TransportPaths {
    pub(crate) receive: PathBuf,
    pub(crate) send: PathBuf,
    pub(crate) lock: PathBuf,
}

impl TransportPaths {
    pub(crate) fn from_receive(receive: PathBuf) -> Self {
        Self {
            send: companion_path(&receive, ".send"),
            lock: companion_path(&receive, ".lock"),
            receive,
        }
    }

    pub(crate) fn canonical(&self) -> io::Result<Vec<PathBuf>> {
        let Self {
            receive,
            send,
            lock,
        } = self;
        [receive, send, lock].iter().map(fs::canonicalize).collect()
    }
}

fn companion_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value: OsString = path.as_os_str().to_owned();
    value.push(suffix);
    PathBuf::from(value)
}

struct OwnedPath {
    path: PathBuf,
    kind: TransportKind,
    device: u64,
    inode: u64,
}

#[derive(Clone, Copy)]
enum TransportKind {
    Fifo,
    Lock,
}

impl TransportKind {
    fn matches(self, metadata: &fs::Metadata) -> bool {
        match self {
            Self::Fifo => metadata.file_type().is_fifo(),
            Self::Lock => metadata.file_type().is_file(),
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Fifo => "FIFO",
            Self::Lock => "regular lock file",
        }
    }
}

impl OwnedPath {
    fn adopt(path: PathBuf, kind: TransportKind) -> io::Result<Self> {
        let metadata = fs::symlink_metadata(&path)?;
        if !kind.matches(&metadata) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} changed while initializing; expected {}",
                    path.display(),
                    kind.description()
                ),
            ));
        }
        Ok(Self {
            path,
            kind,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    // Linux can hand a freed inode number straight to a replacement file, so
    // the device and inode alone do not identify the path we created.
    fn is_same(&self, metadata: &fs::Metadata) -> bool {
        let Self {
            path: _,
            kind,
            device,
            inode,
        } = self;
        metadata.dev() == *device && metadata.ino() == *inode && kind.matches(metadata)
    }

    fn is_unchanged(&self) -> bool {
        fs::symlink_metadata(&self.path).is_ok_and(|metadata| self.is_same(&metadata))
    }

    fn remove_if_unchanged(&self) {
        let Ok(metadata) = fs::symlink_metadata(&self.path) else {
            return;
        };
        if !self.is_same(&metadata) {
            tracing::warn!(path = %self.path.display(), "transport path changed; leaving it in place");
            return;
        }
        if let Err(error) = fs::remove_file(&self.path)
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(path = %self.path.display(), %error, "could not remove transport path");
        }
    }
}

pub(crate) struct ServerTransport {
    paths: TransportPaths,
    owned: Vec<OwnedPath>,
}

impl ServerTransport {
    pub(crate) fn create(receive: &Path) -> io::Result<Self> {
        let receive = ensure_fifo(receive)?;
        let paths = TransportPaths::from_receive(receive);
        ensure_fifo(&paths.send)?;
        ensure_lock_file(&paths.lock)?;
        let owned = [
            (&paths.receive, TransportKind::Fifo),
            (&paths.send, TransportKind::Fifo),
            (&paths.lock, TransportKind::Lock),
        ]
        .into_iter()
        .map(|(path, kind)| OwnedPath::adopt(path.clone(), kind))
        .collect::<io::Result<_>>()?;
        Ok(Self { paths, owned })
    }

    pub(crate) fn paths(&self) -> &TransportPaths {
        &self.paths
    }

    pub(crate) fn open_send_reader(&self) -> io::Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open(&self.paths.send)?;
        verify_open_file(&file, &self.paths.send, TransportKind::Fifo)?;
        Ok(file)
    }

    pub(crate) async fn wait_until_broken(&self) -> io::Error {
        let mut interval = tokio::time::interval(TRANSPORT_CHECK_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if let Some(missing) = self.owned.iter().find(|owned| !owned.is_unchanged()) {
                return io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "transport path {} was deleted or replaced; stopping server",
                        missing.path.display()
                    ),
                );
            }
        }
    }
}

impl Drop for ServerTransport {
    fn drop(&mut self) {
        for owned in self.owned.iter().rev() {
            owned.remove_if_unchanged();
        }
    }
}

pub(crate) struct AdvisoryLock {
    file: File,
}

impl AdvisoryLock {
    pub(crate) fn acquire(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        verify_open_file(&file, path, TransportKind::Lock)?;
        file.lock()?;
        verify_open_file(&file, path, TransportKind::Lock)?;
        Ok(Self { file })
    }

    fn acquire_or_create(path: &Path) -> io::Result<Self> {
        ensure_lock_file(path)?;
        Self::acquire(path)
    }
}

impl Drop for AdvisoryLock {
    fn drop(&mut self) {
        if let Err(error) = self.file.unlock() {
            tracing::warn!(%error, "could not release transport lock");
        }
    }
}

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

    pub(crate) fn start_delivery(&self, paths: TransportPaths) -> DeliveryTask {
        let queue = self.clone();
        let task = tokio::spawn(async move {
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
                let paths_for_write = paths.clone();
                match tokio::task::spawn_blocking(move || {
                    queue_for_write.try_deliver(&paths_for_write)
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
        DeliveryTask { task }
    }

    fn try_deliver(&self, paths: &TransportPaths) -> io::Result<()> {
        let _lock = AdvisoryLock::acquire(&paths.lock)?;
        let mut records = self.records.lock().expect("comment queue poisoned");
        if records.is_empty() {
            return Ok(());
        }

        let mut fifo = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open(&paths.receive)?;
        verify_open_file(&fifo, &paths.receive, TransportKind::Fifo)?;

        while let Some(record) = records.front() {
            let mut bytes = serde_json::to_vec(record).map_err(io::Error::other)?;
            bytes.push(b'\n');
            write_nonblocking(&mut fifo, &bytes)?;
            records.pop_front();
        }
        Ok(())
    }
}

pub(crate) struct DeliveryTask {
    task: JoinHandle<()>,
}

impl Drop for DeliveryTask {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(crate) fn write_nonblocking(writer: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
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

pub(crate) fn receive_batch(receive_path: &Path) -> io::Result<Vec<u8>> {
    let receive = ensure_fifo(receive_path)?;
    let paths = TransportPaths::from_receive(receive);
    ensure_lock_file(&paths.lock)?;
    let mut received = Vec::new();
    let mut fifo = open_receive_reader(&paths.receive)?;

    loop {
        read_delivery_session(&mut fifo, &paths.receive, &mut received)?;

        let _lock = AdvisoryLock::acquire_or_create(&paths.lock)?;
        let mut drain = open_or_create_receive_reader(&paths.receive)?;
        drop(fifo);
        drain_available(&mut drain, &mut received)?;
        drop(drain);

        if let Some(last_newline) = received.iter().rposition(|byte| *byte == b'\n') {
            received.truncate(last_newline + 1);
            return Ok(received);
        }
        fifo = open_or_create_receive_reader(&paths.receive)?;
    }
}

fn read_delivery_session(fifo: &mut File, path: &Path, output: &mut Vec<u8>) -> io::Result<()> {
    let mut writer_seen = false;
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        match fifo.read(&mut buffer) {
            Ok(0) if writer_seen => return Ok(()),
            Ok(0) => {
                if verify_open_file(fifo, path, TransportKind::Fifo).is_err() {
                    *fifo = open_or_create_receive_reader(path)?;
                }
                thread::sleep(RECEIVE_POLL_INTERVAL);
            }
            Ok(read) => {
                writer_seen = true;
                output.extend_from_slice(buffer.get(..read).unwrap_or_default());
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                writer_seen = true;
                thread::sleep(RECEIVE_POLL_INTERVAL);
            }
            Err(error) => return Err(error),
        }
    }
}

fn open_or_create_receive_reader(path: &Path) -> io::Result<File> {
    ensure_fifo(path)?;
    open_receive_reader(path)
}

fn open_receive_reader(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)?;
    verify_open_file(&file, path, TransportKind::Fifo)?;
    Ok(file)
}

fn drain_available(reader: &mut File, output: &mut Vec<u8>) -> io::Result<()> {
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => output.extend_from_slice(buffer.get(..read).unwrap_or_default()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error),
        }
    }
}

pub(crate) fn open_send_writer(receive_path: &Path) -> io::Result<(AdvisoryLock, File)> {
    let receive = absolute_path(receive_path)?;
    let paths = TransportPaths::from_receive(receive);
    let writer = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(&paths.send)
        .map_err(offline_error)?;
    verify_open_file(&writer, &paths.send, TransportKind::Fifo).map_err(offline_error)?;
    let lock = AdvisoryLock::acquire(&paths.lock).map_err(offline_error)?;
    Ok((lock, writer))
}

fn verify_open_file(file: &File, path: &Path, kind: TransportKind) -> io::Result<()> {
    let opened = file.metadata()?;
    let current = fs::symlink_metadata(path)?;
    if !kind.matches(&opened)
        || !kind.matches(&current)
        || opened.dev() != current.dev()
        || opened.ino() != current.ino()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} changed while opening; expected {}",
                path.display(),
                kind.description()
            ),
        ));
    }
    Ok(())
}

fn offline_error(error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("no Komtar server is reading agent messages: {error}"),
    )
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
            match mkfifo(path, Mode::S_IRUSR | Mode::S_IWUSR) {
                Ok(()) => {}
                Err(nix::errno::Errno::EEXIST) => return ensure_fifo(path),
                Err(error) => return Err(io::Error::other(error)),
            }
        }
        Err(error) => return Err(error),
    }
    set_private_permissions(path)?;
    absolute_path(path)
}

fn ensure_lock_file(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "{} exists but is not a regular lock file; refusing to overwrite it",
                        path.display()
                    ),
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)
            {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    return ensure_lock_file(path);
                }
                Err(error) => return Err(error),
            }
        }
        Err(error) => return Err(error),
    }
    set_private_permissions(path)
}

fn set_private_permissions(path: &Path) -> io::Result<()> {
    fchmodat(
        AT_FDCWD,
        path,
        Mode::S_IRUSR | Mode::S_IWUSR,
        FchmodatFlags::NoFollowSymlink,
    )
    .map_err(io::Error::other)
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
        os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink},
        thread,
        time::{Duration, Instant},
    };

    use serde::Deserialize;
    use tempfile::tempdir;

    use crate::model::{CommentRecord, PageContext, Point, PointerContext, Size, TargetContext};

    use super::{
        AdvisoryLock, CommentQueue, OwnedPath, ServerTransport, TransportKind, TransportPaths,
        ensure_fifo, ensure_lock_file, is_reader_absent, receive_batch, write_nonblocking,
    };

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
    fn server_transport_creates_companions_and_cleans_them_up() {
        let temporary = tempdir().expect("temp directory");
        let fifo = temporary.path().join("feedback.pipe");
        let paths = {
            let transport = ServerTransport::create(&fifo).expect("server transport");
            let paths = transport.paths().clone();
            assert!(
                fs::metadata(&paths.receive)
                    .expect("receive")
                    .file_type()
                    .is_fifo()
            );
            assert!(
                fs::metadata(&paths.send)
                    .expect("send")
                    .file_type()
                    .is_fifo()
            );
            assert!(fs::metadata(&paths.lock).expect("lock").is_file());
            paths
        };
        assert!(!paths.receive.exists());
        assert!(!paths.send.exists());
        assert!(!paths.lock.exists());
    }

    #[test]
    fn treats_a_reused_inode_with_another_type_as_replaced() {
        let temporary = tempdir().expect("temp directory");
        let path = temporary.path().join("feedback.pipe");
        fs::write(&path, "replacement owned by the user").expect("write fixture");
        let metadata = fs::symlink_metadata(&path).expect("fixture metadata");
        // Simulate a FIFO whose inode number was reused by the regular file.
        let owned = OwnedPath {
            path: path.clone(),
            kind: TransportKind::Fifo,
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        assert!(!owned.is_unchanged());
        owned.remove_if_unchanged();
        assert_eq!(
            fs::read_to_string(path).expect("replacement survives"),
            "replacement owned by the user"
        );
    }

    #[test]
    fn constructs_companion_paths_by_appending_suffixes() {
        let paths = TransportPaths::from_receive("feedback.pipe".into());
        assert_eq!(paths.send, std::path::Path::new("feedback.pipe.send"));
        assert_eq!(paths.lock, std::path::Path::new("feedback.pipe.lock"));
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
    fn refuses_symlinked_transport_paths() {
        let temporary = tempdir().expect("temp directory");
        let fifo_target = temporary.path().join("actual.fifo");
        let fifo_link = temporary.path().join("linked.fifo");
        ensure_fifo(&fifo_target).expect("FIFO target");
        symlink(&fifo_target, &fifo_link).expect("FIFO symlink");
        assert!(ensure_fifo(&fifo_link).is_err());

        let lock_target = temporary.path().join("actual.lock");
        let lock_link = temporary.path().join("linked.lock");
        ensure_lock_file(&lock_target).expect("lock target");
        symlink(&lock_target, &lock_link).expect("lock symlink");
        assert!(AdvisoryLock::acquire(&lock_link).is_err());
    }

    #[test]
    fn rejects_comments_after_the_queue_reaches_capacity() {
        let queue = CommentQueue::new();
        for index in 0..crate::model::MAX_QUEUE_RECORDS {
            queue
                .enqueue(record(&format!("comment {index}")))
                .expect("queue within capacity");
        }
        let error = queue
            .enqueue(record("overflow"))
            .expect_err("queue must reject overflow");
        assert_eq!(error.status, hyper::StatusCode::INSUFFICIENT_STORAGE);
        assert_eq!(queue.pending(), crate::model::MAX_QUEUE_RECORDS);
    }

    #[test]
    fn retains_a_record_until_a_reader_arrives() {
        let temporary = tempdir().expect("temp directory");
        let transport =
            ServerTransport::create(&temporary.path().join("comments.fifo")).expect("transport");
        let queue = CommentQueue::new();
        queue.enqueue(record("waiting")).expect("enqueue");
        let error = queue
            .try_deliver(transport.paths())
            .expect_err("no FIFO reader is present");
        assert!(is_reader_absent(&error));
        assert_eq!(queue.pending(), 1);
    }

    #[test]
    fn drains_waiting_records_in_submission_order() {
        let temporary = tempdir().expect("temp directory");
        let transport =
            ServerTransport::create(&temporary.path().join("comments.fifo")).expect("transport");
        let paths = transport.paths().clone();
        let queue = CommentQueue::new();
        queue.enqueue(record("first")).expect("first record");
        queue.enqueue(record("second")).expect("second record");

        let fifo_for_reader = paths.receive.clone();
        let reader = thread::spawn(move || fs::read_to_string(fifo_for_reader));
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match queue.try_deliver(&paths) {
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

    #[test]
    fn receive_follows_a_fifo_replaced_while_waiting() {
        let temporary = tempdir().expect("temp directory");
        let receive_path = temporary.path().join("comments.fifo");
        ensure_fifo(&receive_path).expect("initial FIFO");
        let paths = TransportPaths::from_receive(receive_path.clone());
        let receive_for_thread = receive_path.clone();
        let receiver = thread::spawn(move || receive_batch(&receive_for_thread));

        let deadline = Instant::now() + Duration::from_secs(2);
        while !paths.lock.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(paths.lock.exists());
        fs::remove_file(&receive_path).expect("unlink original FIFO");
        fs::remove_file(&paths.lock).expect("unlink original lock");
        ensure_fifo(&receive_path).expect("replacement FIFO");
        ensure_lock_file(&paths.lock).expect("replacement lock");

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut writer = loop {
            match fs::OpenOptions::new()
                .write(true)
                .custom_flags(nix::libc::O_NONBLOCK)
                .open(&receive_path)
            {
                Ok(writer) => break writer,
                Err(error) if is_reader_absent(&error) && Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("could not open replacement FIFO: {error}"),
            }
        };
        write_nonblocking(&mut writer, b"replacement record\n").expect("replacement delivery");
        drop(writer);

        let payload = receiver
            .join()
            .expect("receiver thread")
            .expect("receive replacement batch");
        assert_eq!(payload, b"replacement record\n");
    }

    #[test]
    fn coordinated_receive_shutdown_does_not_lose_a_racing_record() {
        let temporary = tempdir().expect("temp directory");
        let transport =
            ServerTransport::create(&temporary.path().join("comments.fifo")).expect("transport");
        let paths = transport.paths().clone();
        let queue = CommentQueue::new();
        queue.enqueue(record("first")).expect("first record");

        let first_path = paths.receive.clone();
        let first_receiver = thread::spawn(move || receive_batch(&first_path));
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match queue.try_deliver(&paths) {
                Ok(()) => break,
                Err(error) if is_reader_absent(&error) && Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("first FIFO delivery failed: {error}"),
            }
        }

        queue.enqueue(record("second")).expect("second record");
        match queue.try_deliver(&paths) {
            Ok(()) => {}
            Err(error) if is_reader_absent(&error) => {}
            Err(error) => panic!("racing FIFO delivery failed: {error}"),
        }
        let mut payload = first_receiver
            .join()
            .expect("first receiver thread")
            .expect("first receive batch");

        if queue.pending() > 0 {
            let second_path = paths.receive.clone();
            let second_receiver = thread::spawn(move || receive_batch(&second_path));
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match queue.try_deliver(&paths) {
                    Ok(()) => break,
                    Err(error) if is_reader_absent(&error) && Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("second FIFO delivery failed: {error}"),
                }
            }
            payload.extend(
                second_receiver
                    .join()
                    .expect("second receiver thread")
                    .expect("second receive batch"),
            );
        }

        let comments: Vec<String> = payload
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                serde_json::from_slice::<DeliveredRecord>(line)
                    .map(|record| record.comment)
                    .map_err(io::Error::other)
            })
            .collect::<io::Result<_>>()
            .expect("delivered records");
        assert_eq!(comments, ["first", "second"]);
        assert_eq!(queue.pending(), 0);
    }

    struct PartialWriter {
        output: Vec<u8>,
        calls: usize,
    }

    impl io::Write for PartialWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            if self.calls == 2 {
                return Err(io::Error::from(io::ErrorKind::WouldBlock));
            }
            let written = bytes.len().min(3);
            self.output
                .extend_from_slice(bytes.get(..written).unwrap_or_default());
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn retries_partial_and_would_block_writes() {
        let mut writer = PartialWriter {
            output: Vec::new(),
            calls: 0,
        };
        write_nonblocking(&mut writer, b"complete record").expect("write succeeds");
        assert_eq!(writer.output, b"complete record");
        assert!(writer.calls > 2);
    }
}
