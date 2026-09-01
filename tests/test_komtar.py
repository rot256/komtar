"""HTTP and browser integration tests for Komtar's proxy and static server."""

from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import base64
import hashlib
import json
import os
import socket
import subprocess
import tempfile
import threading
import time
from pathlib import Path
from urllib.error import HTTPError
from urllib.request import HTTPRedirectHandler, Request, build_opener, urlopen

import pytest
from playwright.sync_api import Browser, Page, expect, sync_playwright

ROOT = Path(__file__).resolve().parent.parent
TARGET_DIR = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
FIXTURE_HTML = (ROOT / "tests/fixtures/site/index.html").read_bytes()
HOST = "127.0.0.1"
STARTUP_TIMEOUT = 20.0


def available_port() -> int:
    with socket.socket() as sock:
        sock.bind((HOST, 0))
        return int(sock.getsockname()[1])


def wait_for_port(process: subprocess.Popen[str], port: int) -> None:
    deadline = time.monotonic() + STARTUP_TIMEOUT
    while time.monotonic() < deadline:
        if process.poll() is not None:
            stdout, _ = process.communicate()
            pytest.fail(f"komtar exited with {process.returncode}:\n{stdout}")
        with socket.socket() as sock:
            sock.settimeout(0.1)
            if sock.connect_ex((HOST, port)) == 0:
                return
        time.sleep(0.05)
    pytest.fail(f"komtar did not listen on port {port}")


class FixtureHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, _format: str, *_args: object) -> None:
        pass

    def send_bytes(self, status: int, content_type: str, body: bytes) -> None:
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:
        if self.path == "/socket" and self.headers.get("Upgrade", "").lower() == "websocket":
            self.handle_websocket()
        elif self.path == "/":
            self.send_bytes(200, "text/html; charset=utf-8", FIXTURE_HTML)
        elif self.path == "/without-body":
            self.send_bytes(200, "text/html", b"<main>No body close</main>")
        elif self.path == "/large":
            self.send_bytes(200, "text/html", b"x" * (2 * 1024 * 1024 + 1))
        elif self.path == "/asset.bin":
            self.send_bytes(200, "application/octet-stream", bytes(range(256)))
        elif self.path == "/redirect":
            address, port = self.server.server_address
            self.send_response(302)
            self.send_header("Location", f"http://{address}:{port}/next")
            self.send_header("Content-Length", "0")
            self.end_headers()
        else:
            self.send_bytes(404, "text/plain", b"missing")

    def do_HEAD(self) -> None:
        if self.path == "/":
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(FIXTURE_HTML)))
            self.end_headers()
        else:
            self.send_response(404)
            self.send_header("Content-Length", "0")
            self.end_headers()

    def handle_websocket(self) -> None:
        key = self.headers["Sec-WebSocket-Key"]
        accept = base64.b64encode(
            hashlib.sha1(
                (key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()
            ).digest()
        ).decode()
        self.send_response(101)
        self.send_header("Upgrade", "websocket")
        self.send_header("Connection", "Upgrade")
        self.send_header("Sec-WebSocket-Accept", accept)
        self.end_headers()
        self.wfile.flush()

        header = self.rfile.read(2)
        if len(header) != 2:
            return
        length = header[1] & 0x7F
        if length == 126:
            length = int.from_bytes(self.rfile.read(2), "big")
        elif length == 127:
            length = int.from_bytes(self.rfile.read(8), "big")
        mask = self.rfile.read(4)
        encoded = self.rfile.read(length)
        payload = bytes(value ^ mask[index % 4] for index, value in enumerate(encoded))
        self.wfile.write(bytes([0x81, len(payload)]) + payload)
        self.wfile.flush()
        self.close_connection = True

    def do_POST(self) -> None:
        length = int(self.headers.get("Content-Length", "0"))
        self.send_bytes(200, "application/octet-stream", self.rfile.read(length))


@contextmanager
def upstream_server():
    server = ThreadingHTTPServer((HOST, 0), FixtureHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://{HOST}:{server.server_port}"
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


@contextmanager
def komtar(fifo: Path, *source: str):
    port = available_port()
    arguments = [str(TARGET_DIR / "debug/komtar"), *source]
    arguments.extend(["--listen", f"{HOST}:{port}", "--fifo", str(fifo)])
    process = subprocess.Popen(
        arguments,
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    try:
        wait_for_port(process, port)
        yield f"http://{HOST}:{port}"
    finally:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


@pytest.fixture(scope="session", autouse=True)
def build_binary() -> None:
    subprocess.run(["cargo", "build"], cwd=ROOT, check=True)


@pytest.fixture(scope="session")
def browser() -> Browser:
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch()
        yield browser
        browser.close()


@pytest.fixture()
def page(browser: Browser) -> Page:
    page = browser.new_page(viewport={"width": 1280, "height": 720})
    yield page
    page.close()


def test_proxy_injects_only_html_and_preserves_http_behavior() -> None:
    with tempfile.TemporaryDirectory() as temporary, upstream_server() as upstream:
        fifo = Path(temporary) / "comments.fifo"
        with komtar(fifo, "proxy", upstream) as proxy:
            direct = urlopen(upstream).read()
            annotated_response = urlopen(proxy)
            annotated = annotated_response.read()
            assert b'id="komtar"' not in direct
            assert b'/_komtar/client.js' in annotated
            annotated_head = urlopen(Request(proxy, method="HEAD"))
            assert int(annotated_head.headers["Content-Length"]) == len(annotated)
            assert b'/_komtar/client.js' in urlopen(proxy + "/without-body").read()
            assert urlopen(proxy + "/asset.bin").read() == bytes(range(256))
            large = urlopen(proxy + "/large").read()
            assert len(large) == 2 * 1024 * 1024 + 1
            assert b'/_komtar/client.js' not in large

            payload = b"method body survives"
            echoed = urlopen(Request(proxy + "/echo", data=payload, method="POST")).read()
            assert echoed == payload

            class NoRedirect(HTTPRedirectHandler):
                def redirect_request(self, *_args: object, **_kwargs: object):
                    return None

            with pytest.raises(HTTPError) as raised:
                build_opener(NoRedirect).open(proxy + "/redirect")
            assert raised.value.code == 302
            assert raised.value.headers["Location"] == "/next"


def test_proxy_forwards_websocket_upgrades(page: Page) -> None:
    with tempfile.TemporaryDirectory() as temporary, upstream_server() as upstream:
        fifo = Path(temporary) / "comments.fifo"
        with komtar(fifo, upstream) as proxy:
            echoed = page.evaluate(
                """(base) => new Promise((resolve, reject) => {
                  const socket = new WebSocket(base.replace('http:', 'ws:') + '/socket');
                  const timer = setTimeout(() => reject(new Error('WebSocket timeout')), 3000);
                  socket.addEventListener('open', () => socket.send('hmr-ping'));
                  socket.addEventListener('message', (event) => {
                    clearTimeout(timer);
                    resolve(event.data);
                    socket.close();
                  });
                  socket.addEventListener('error', () => reject(new Error('WebSocket error')));
                })""",
                proxy,
            )
            assert echoed == "hmr-ping"


def test_browser_queues_selection_context_coordinates_and_multiple_edits(
    page: Page,
) -> None:
    with tempfile.TemporaryDirectory() as temporary, upstream_server() as upstream:
        fifo = Path(temporary) / "comments.fifo"
        with komtar(fifo, upstream) as proxy:
            page.goto(proxy, wait_until="networkidle")
            expect(page.locator("#komtar")).to_be_attached()
            expect(page.locator("#komtar #badge")).to_have_text("0 queued")

            selected_text = page.evaluate(
                """() => {
                  const paragraph = document.querySelector('#intro');
                  const node = paragraph.firstChild;
                  const phrase = 'a specific sentence';
                  const start = node.textContent.indexOf(phrase);
                  const range = document.createRange();
                  range.setStart(node, start);
                  range.setEnd(node, start + phrase.length);
                  const selection = window.getSelection();
                  selection.removeAllRanges();
                  selection.addRange(range);
                  const selected = selection.toString();
                  const rect = paragraph.getBoundingClientRect();
                  paragraph.dispatchEvent(new MouseEvent('contextmenu', {
                    bubbles: true, cancelable: true,
                    clientX: rect.left + 18, clientY: rect.top + 12,
                  }));
                  return selected;
                }"""
            )
            dialog = page.locator("#komtar-dialog")
            expect(dialog).to_be_visible()
            expect(page.get_by_text("Suggest Edit:", exact=True)).to_be_visible()
            expect(page.locator("#komtar #selection-text")).to_have_text(selected_text)
            page.locator("#komtar textarea").fill("Rewrite this sentence.")
            page.locator("#komtar textarea").press("Enter")
            expect(dialog).to_be_hidden()
            expect(page.locator("#komtar #badge")).to_have_text("1 queued")

            page.locator("#action").click(button="right", position={"x": 5, "y": 4})
            page.locator("#komtar textarea").fill("Use a clearer label.")
            page.get_by_role("button", name="Queue comment").click()
            expect(page.locator("#komtar #badge")).to_have_text("2 queued")

            with ThreadPoolExecutor(max_workers=1) as pool:
                delivered = pool.submit(fifo.read_text, encoding="utf-8")
                payload = delivered.result(timeout=5)
            records = [json.loads(line) for line in payload.splitlines()]
            assert [record["comment"] for record in records] == [
                "Rewrite this sentence.",
                "Use a clearer label.",
            ]
            assert records[0]["version"] == 1
            assert records[0]["target"]["selectedText"] == selected_text
            assert records[0]["target"]["selector"] == "#intro"
            assert records[0]["pointer"]["target"]["x"] == pytest.approx(18, abs=1)
            assert records[0]["pointer"]["target"]["y"] == pytest.approx(12, abs=1)
            expect(page.locator("#komtar #badge")).to_have_text("0 queued")


def test_clicking_outside_closes_without_queueing(page: Page) -> None:
    with tempfile.TemporaryDirectory() as temporary, upstream_server() as upstream:
        fifo = Path(temporary) / "comments.fifo"
        with komtar(fifo, upstream) as proxy:
            page.goto(proxy, wait_until="networkidle")
            page.locator("#intro").click(button="right")
            dialog = page.locator("#komtar-dialog")
            expect(dialog).to_be_visible()
            page.mouse.click(1, 1)
            expect(dialog).to_be_hidden()
            expect(page.locator("#komtar #badge")).to_have_text("0 queued")


def test_serve_routes_static_files_and_injects_the_live_client() -> None:
    with tempfile.TemporaryDirectory() as temporary:
        site = Path(temporary) / "site"
        site.mkdir()
        (site / "index.html").write_text("<body><h1>Home</h1></body>", encoding="utf-8")
        (site / "plain.txt").write_text("plain file", encoding="utf-8")
        docs = site / "docs"
        docs.mkdir()
        (docs / "index.html").write_text("<h1>Docs</h1>", encoding="utf-8")
        (site / ".env").write_text("SECRET=not-for-http", encoding="utf-8")
        (site / "empty-dir").mkdir()
        outside = Path(temporary) / "outside.txt"
        outside.write_text("outside", encoding="utf-8")
        (site / "outside.txt").symlink_to(outside)
        fifo = site / ".komtar"

        with komtar(fifo, "serve", str(site)) as server:
            response = urlopen(server)
            body = response.read()
            assert response.headers["Cache-Control"] == "no-store"
            assert b'/_komtar/client.js?live=' in body
            revision = body.split(b'client.js?live=', 1)[1].split(b'"', 1)[0]
            with urlopen(server + "/_komtar/api/reload", timeout=2) as events:
                assert events.readline() == b"data: " + revision + b"\n"
            assert urlopen(server + "/plain.txt").read() == b"plain file"
            assert b"Docs" in urlopen(server + "/docs").read()
            head_html = urlopen(Request(server, method="HEAD"))
            assert int(head_html.headers["Content-Length"]) == len(body)

            head = urlopen(Request(server + "/plain.txt", method="HEAD"))
            assert head.read() == b""
            with pytest.raises(HTTPError) as missing:
                urlopen(server + "/missing")
            assert missing.value.code == 404
            for hidden_path in (
                "/.komtar",
                "/.env",
                "/outside.txt",
                "/plain.txt/nested",
                "/empty-dir/",
                "/%2e%2e/%2e%2e/not-in-root.txt",
            ):
                with pytest.raises(HTTPError) as hidden:
                    urlopen(server + hidden_path, timeout=2)
                assert hidden.value.code == 404
            with pytest.raises(HTTPError) as wrong_method:
                urlopen(Request(server + "/plain.txt", data=b"no", method="POST"))
            assert wrong_method.value.code == 405
            assert wrong_method.value.headers["Allow"] == "GET, HEAD"


def test_live_reload_waits_for_an_open_edit_and_keeps_it_queueable(page: Page) -> None:
    with tempfile.TemporaryDirectory() as temporary:
        site = Path(temporary) / "site"
        site.mkdir()
        index = site / "index.html"
        index.write_text(
            '<body><h1 id="title">Version one</h1></body>', encoding="utf-8"
        )
        fifo = site / ".komtar"

        with komtar(fifo, "serve", str(site)) as server:
            page.add_init_script(
                """sessionStorage.setItem(
                  'komtar-test-loads',
                  String(Number(sessionStorage.getItem('komtar-test-loads') || '0') + 1),
                );"""
            )
            page.goto(server, wait_until="domcontentloaded")
            expect(page.locator("#title")).to_have_text("Version one")

            page.locator("#title").click(button="right")
            textarea = page.locator("#komtar textarea")
            textarea.fill("Keep this draft while files change.")
            index.write_text(
                '<body><h1 id="title">Version two</h1></body>', encoding="utf-8"
            )

            expect(page.locator("#komtar #reload-notice")).to_be_visible()
            expect(textarea).to_have_value("Keep this draft while files change.")
            expect(page.locator("#title")).to_have_text("Version one")
            assert page.evaluate("sessionStorage.getItem('komtar-test-loads')") == "1"
            page.locator("body").click(button="right", position={"x": 2, "y": 2})
            expect(textarea).to_have_value("Keep this draft while files change.")
            assert page.evaluate("sessionStorage.getItem('komtar-test-loads')") == "1"

            page.get_by_role("button", name="Queue comment").click()
            expect(page.locator("#title")).to_have_text("Version two")
            assert page.evaluate("sessionStorage.getItem('komtar-test-loads')") == "2"

            with ThreadPoolExecutor(max_workers=1) as pool:
                delivered = pool.submit(fifo.read_text, encoding="utf-8")
                record = json.loads(delivered.result(timeout=5))
            assert record["comment"] == "Keep this draft while files change."

            time.sleep(0.5)
            assert page.evaluate("sessionStorage.getItem('komtar-test-loads')") == "2"
            (site / ".ignored").write_text("hidden change", encoding="utf-8")
            time.sleep(0.5)
            assert page.evaluate("sessionStorage.getItem('komtar-test-loads')") == "2"
            index.write_text(
                '<body><h1 id="title">Version three</h1></body>', encoding="utf-8"
            )
            expect(page.locator("#title")).to_have_text("Version three")
            assert page.evaluate("sessionStorage.getItem('komtar-test-loads')") == "3"
