use std::{
    collections::VecDeque,
    fs::File,
    io::{self, Read},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, TagEnd, html};
use serde::Serialize;
use tokio::{sync::broadcast, task::JoinHandle};

use crate::{
    fifo::{open_send_writer, write_nonblocking},
    model::{AgentMessage, CommentRecord, PageContext, Point, PointerContext, Size, TargetContext},
};

pub(crate) const MAX_AGENT_RECORD_BYTES: usize = 128 * 1024;
const MAX_AGENT_HISTORY: usize = 500;
const REPLAY_WINDOW: Duration = Duration::from_secs(10 * 60);
const INBOX_POLL_INTERVAL: Duration = Duration::from_millis(25);
const BROADCAST_CAPACITY: usize = 512;

type Clock = Arc<dyn Fn() -> Duration + Send + Sync>;

#[derive(Clone, Debug, Serialize)]
pub(crate) struct AgentEvent {
    pub(crate) id: String,
    pub(crate) html: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) anchor: Option<String>,
}

struct TimedEvent {
    received_at: Duration,
    event: Arc<AgentEvent>,
}

struct AgentHistory {
    events: VecDeque<TimedEvent>,
    clock: Clock,
}

impl AgentHistory {
    fn now(&self) -> Duration {
        (self.clock)()
    }

    fn push(&mut self, event: Arc<AgentEvent>) {
        if self.events.len() == MAX_AGENT_HISTORY {
            self.events.pop_front();
        }
        self.events.push_back(TimedEvent {
            received_at: self.now(),
            event,
        });
    }

    fn snapshot(&self) -> Vec<Arc<AgentEvent>> {
        let now = self.now();
        self.events
            .iter()
            .filter(|timed| now.saturating_sub(timed.received_at) <= REPLAY_WINDOW)
            .map(|timed| timed.event.clone())
            .collect()
    }
}

#[derive(Clone)]
pub(crate) struct AgentHub {
    history: Arc<Mutex<AgentHistory>>,
    sender: broadcast::Sender<Arc<AgentEvent>>,
}

impl AgentHub {
    pub(crate) fn new() -> Self {
        let started = Instant::now();
        Self::with_clock(Arc::new(move || started.elapsed()))
    }

    fn with_clock(clock: Clock) -> Self {
        let (sender, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            history: Arc::new(Mutex::new(AgentHistory {
                events: VecDeque::new(),
                clock,
            })),
            sender,
        }
    }

    pub(crate) fn publish(&self, input: AgentMessage) -> Arc<AgentEvent> {
        let AgentMessage {
            version: _,
            message,
            anchor,
        } = input;
        let event = Arc::new(AgentEvent {
            id: uuid::Uuid::new_v4().to_string(),
            html: render_markdown(&message),
            anchor,
        });
        self.history
            .lock()
            .expect("agent history poisoned")
            .push(event.clone());
        let _ = self.sender.send(event.clone());
        event
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<Arc<AgentEvent>> {
        self.sender.subscribe()
    }

    pub(crate) fn snapshot(&self) -> Vec<Arc<AgentEvent>> {
        self.history
            .lock()
            .expect("agent history poisoned")
            .snapshot()
    }
}

pub(crate) struct InboxTask {
    task: JoinHandle<()>,
}

impl InboxTask {
    pub(crate) fn start(mut file: File, hub: AgentHub) -> Self {
        let task = tokio::spawn(async move {
            let mut decoder = NdjsonDecoder::new();
            let mut buffer = [0_u8; 8 * 1024];
            loop {
                match file.read(&mut buffer) {
                    Ok(0) => tokio::time::sleep(INBOX_POLL_INTERVAL).await,
                    Ok(read) => {
                        for line in decoder.push(buffer.get(..read).unwrap_or_default()) {
                            ingest_line(&hub, &line);
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        tokio::time::sleep(INBOX_POLL_INTERVAL).await;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "agent message FIFO reader failed");
                        tokio::time::sleep(INBOX_POLL_INTERVAL).await;
                    }
                }
            }
        });
        Self { task }
    }
}

impl Drop for InboxTask {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct NdjsonDecoder {
    line: Vec<u8>,
    discarding: bool,
}

impl NdjsonDecoder {
    fn new() -> Self {
        Self {
            line: Vec::new(),
            discarding: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut lines = Vec::new();
        for byte in bytes {
            if *byte == b'\n' {
                if self.discarding {
                    tracing::warn!(
                        limit = MAX_AGENT_RECORD_BYTES,
                        "skipping oversized agent message record"
                    );
                } else {
                    lines.push(std::mem::take(&mut self.line));
                }
                self.line.clear();
                self.discarding = false;
            } else if !self.discarding {
                if self.line.len() == MAX_AGENT_RECORD_BYTES {
                    self.line.clear();
                    self.discarding = true;
                } else {
                    self.line.push(*byte);
                }
            }
        }
        lines
    }
}

fn ingest_line(hub: &AgentHub, line: &[u8]) {
    let input = serde_json::from_slice::<AgentMessage>(line)
        .map_err(|error| format!("invalid JSON: {error}"))
        .and_then(AgentMessage::validate);
    match input {
        Ok(input) => {
            hub.publish(input);
        }
        Err(error) => tracing::warn!(%error, "skipping malformed agent message record"),
    }
}

pub(crate) fn send_message(receive_path: &std::path::Path, input: AgentMessage) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(&input).map_err(io::Error::other)?;
    bytes.push(b'\n');
    let (_lock, mut writer) = open_send_writer(receive_path)?;
    write_nonblocking(&mut writer, &bytes)
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "ordinary Markdown events pass through; only unsafe links, images, and raw HTML differ"
)]
pub(crate) fn render_markdown(markdown: &str) -> String {
    let options = Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    let parser = Parser::new_ext(markdown, options);
    let mut filtered = Vec::new();
    let mut suppressed_links = 0_usize;
    let mut suppressed_images = 0_usize;

    for event in parser {
        match event {
            Event::Start(Tag::Link { ref dest_url, .. }) if !safe_link(dest_url) => {
                suppressed_links += 1;
            }
            Event::End(TagEnd::Link) if suppressed_links > 0 => {
                suppressed_links -= 1;
            }
            Event::Start(Tag::Image { .. }) => {
                suppressed_images += 1;
            }
            Event::End(TagEnd::Image) if suppressed_images > 0 => {
                suppressed_images -= 1;
            }
            Event::Html(raw) | Event::InlineHtml(raw) => {
                filtered.push(Event::Text(CowStr::from(raw.into_string())));
            }
            other => filtered.push(other),
        }
    }

    let mut output = String::new();
    html::push_html(&mut output, filtered.into_iter());
    output
}

fn safe_link(destination: &str) -> bool {
    let destination = destination.trim();
    let Some(colon) = destination.find(':') else {
        return true;
    };
    let first_separator = destination
        .find(['/', '?', '#'])
        .unwrap_or(destination.len());
    if colon > first_separator {
        return true;
    }
    matches!(
        destination
            .get(..colon)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("http" | "https" | "mailto" | "komtar")
    )
}

pub(crate) fn agent_guidance() -> Result<String, serde_json::Error> {
    let browser_example = browser_example();
    Ok(format!(
        "Run commands from the same project directory as the server.\n\
         1. Run `komtar recv`. It intentionally waits indefinitely when no feedback arrives.\n\
         2. Process every returned browser comment and make the requested edits.\n\
         3. Reply with `komtar send` only when a comment explicitly asks a question that requires an answer.\n\
         4. Invoke `komtar recv` directly again after each work batch. Do not wrap it in a shell loop.\n\
         Never send acknowledgements, progress reports, completion notices, or done messages.\n\
         For an element-specific answer, reuse the question's nonblank target.selector with `komtar send --anchor CSS_SELECTOR`. A blank selector is a page-level comment and must remain unanchored.\n\
         Comments on an agent response use an opaque `komtar-agent:` selector; reuse it exactly when anchoring the requested answer.\n\
         Element links use `[label](komtar:SELECTOR)`. Percent-encode spaces and reserved characters, for example `komtar:%23intro%20.item`.\n\
         `send` and `recv` are the normative API. Raw FIFO access is for interoperability/debugging; direct writers must not write concurrently.\n\n\
         `recv` returns unchanged v1 browser feedback as NDJSON. Important fields are `comment`, `page`, and `target`; use `target.selector` when anchoring a requested answer.\n\n\
         Example `recv` record:\n{}\n",
        serde_json::to_string_pretty(&browser_example)?,
    ))
}

fn browser_example() -> CommentRecord {
    CommentRecord {
        version: 1,
        id: "550e8400-e29b-41d4-a716-446655440000".to_owned(),
        timestamp: "2026-09-01T12:00:00Z".to_owned(),
        comment: "Should this heading be more specific?".to_owned(),
        page: PageContext {
            url: "http://127.0.0.1:3939/".to_owned(),
            title: "Project".to_owned(),
        },
        target: TargetContext {
            selector: "#intro".to_owned(),
            tag: "h2".to_owned(),
            id: Some("intro".to_owned()),
            classes: vec!["item".to_owned()],
            selected_text: None,
            text: "Introduction".to_owned(),
            html: "<h2 id=\"intro\" class=\"item\">Introduction</h2>".to_owned(),
        },
        pointer: PointerContext {
            page: Point { x: 120.0, y: 180.0 },
            viewport: Point { x: 120.0, y: 180.0 },
            target: Point { x: 12.0, y: 8.0 },
            scroll: Point { x: 0.0, y: 0.0 },
            viewport_size: Size {
                width: 1280.0,
                height: 720.0,
            },
            target_size: Size {
                width: 420.0,
                height: 48.0,
            },
            device_pixel_ratio: 1.0,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
        time::Duration,
    };

    use super::{AgentHub, NdjsonDecoder, REPLAY_WINDOW, render_markdown};
    use crate::model::AgentMessage;

    fn message(text: &str) -> AgentMessage {
        AgentMessage::new(text.to_owned(), None).expect("valid agent message")
    }

    #[test]
    fn markdown_escapes_html_and_rejects_unsafe_links() {
        let html = render_markdown(
            "- one\n- two\n\n<script>alert(1)</script>\n\n[bad](JavaScript:alert(1)) [data](data:text/html,bad) [good](https://example.com) [element](komtar:%23intro%20.item) ![alt text](https://example.com/image.png)",
        );
        assert!(html.contains("<ul>"));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(!html.to_ascii_lowercase().contains("href=\"javascript:"));
        assert!(!html.contains("href=\"data:"));
        assert!(html.contains("bad"));
        assert!(html.contains("data"));
        assert!(html.contains("href=\"https://example.com\""));
        assert!(html.contains("href=\"komtar:%23intro%20.item\""));
        assert!(html.contains("alt text"));
        assert!(!html.contains("<img"));
    }

    #[test]
    fn history_uses_injected_time_and_capacity() {
        let now = Arc::new(AtomicU64::new(0));
        let clock_now = now.clone();
        let hub = AgentHub::with_clock(Arc::new(move || {
            Duration::from_secs(clock_now.load(Ordering::SeqCst))
        }));
        hub.publish(message("boundary"));
        now.store(REPLAY_WINDOW.as_secs(), Ordering::SeqCst);
        assert_eq!(hub.snapshot().len(), 1);
        now.store(REPLAY_WINDOW.as_secs() + 1, Ordering::SeqCst);
        assert!(hub.snapshot().is_empty());

        for index in 0..501 {
            hub.publish(message(&format!("message {index}")));
        }
        let snapshot = hub.snapshot();
        assert_eq!(snapshot.len(), 500);
        assert!(
            snapshot
                .first()
                .expect("first event")
                .html
                .contains("message 1")
        );
        assert!(
            snapshot
                .last()
                .expect("last event")
                .html
                .contains("message 500")
        );
    }

    #[test]
    fn decoder_skips_oversized_lines_and_recovers() {
        let mut decoder = NdjsonDecoder::new();
        let mut input = vec![b'x'; super::MAX_AGENT_RECORD_BYTES + 1];
        input.extend_from_slice(b"\n{\"version\":1,\"message\":\"ok\"}\n");
        let lines = decoder.push(&input);
        assert_eq!(lines, [b"{\"version\":1,\"message\":\"ok\"}".to_vec()]);
    }
}
