"""HTTP and browser integration tests for Komtar's proxy and static server."""

import base64
import hashlib
import http.client
import json
import os
import signal
import socket
import subprocess
import tempfile
import threading
import time
from collections.abc import Iterator
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import cast
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

    def log_message(self, format: str, *_args: object) -> None:
        del format

    def send_bytes(self, status: int, content_type: str, body: bytes) -> None:
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:
        if (
            self.path == "/socket"
            and self.headers.get("Upgrade", "").lower() == "websocket"
        ):
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
            address, port = cast(tuple[str, int], self.server.server_address)
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
def upstream_server() -> Iterator[str]:
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
def komtar_process(
    fifo: Path, *source: str
) -> Iterator[tuple[str, subprocess.Popen[str]]]:
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
        yield f"http://{HOST}:{port}", process
    finally:
        if process.poll() is None:
            process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


@contextmanager
def komtar(fifo: Path, *source: str) -> Iterator[str]:
    with komtar_process(fifo, *source) as (url, _process):
        yield url


def run_cli(
    *arguments: str, input_text: str | None = None
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [str(TARGET_DIR / "debug/komtar"), *arguments],
        cwd=ROOT,
        input=input_text,
        capture_output=True,
        text=True,
        timeout=5,
        check=False,
    )


def send_message(fifo: Path, message: str, anchor: str | None = None) -> None:
    arguments = ["send", "--fifo", str(fifo)]
    if anchor is not None:
        arguments.extend(["--anchor", anchor])
    sent = run_cli(*arguments, input_text=message)
    assert sent.returncode == 0, sent.stderr


@pytest.fixture(scope="session", autouse=True)
def build_binary() -> None:
    subprocess.run(["cargo", "build"], cwd=ROOT, check=True)


@pytest.fixture(scope="session")
def browser() -> Iterator[Browser]:
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch()
        yield browser
        browser.close()


@pytest.fixture()
def page(browser: Browser) -> Iterator[Page]:
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
            assert b"/_komtar/client.js" in annotated
            annotated_head = urlopen(Request(proxy, method="HEAD"))
            assert int(annotated_head.headers["Content-Length"]) == len(annotated)
            assert b"/_komtar/client.js" in urlopen(proxy + "/without-body").read()
            assert urlopen(proxy + "/asset.bin").read() == bytes(range(256))
            large = urlopen(proxy + "/large").read()
            assert len(large) == 2 * 1024 * 1024 + 1
            assert b"/_komtar/client.js" not in large

            payload = b"method body survives"
            echoed = urlopen(
                Request(proxy + "/echo", data=payload, method="POST")
            ).read()
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

            delivered = run_cli("recv", "--fifo", str(fifo))
            assert delivered.returncode == 0, delivered.stderr
            payload = delivered.stdout
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
            assert b"/_komtar/client.js?live=" in body
            revision = body.split(b"client.js?live=", 1)[1].split(b'"', 1)[0]
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


def test_help_agent_guidance_transport_ownership_and_deleted_fifo_shutdown(
    tmp_path: Path,
) -> None:
    help_result = run_cli("--help")
    assert help_result.returncode == 0
    assert "komtar recv" in help_result.stdout
    assert "cat .komtar" not in help_result.stdout

    agent_result = run_cli("agent")
    assert agent_result.returncode == 0
    assert "Invoke `komtar recv` directly again" in agent_result.stdout
    assert "Never send acknowledgements" in agent_result.stdout
    assert "target.selector" in agent_result.stdout
    assert "schema" not in agent_result.stdout.lower()
    conflicting = run_cli("http://127.0.0.1:8000", "recv")
    assert conflicting.returncode != 0
    assert "cannot be combined with a helper command" in conflicting.stderr

    fifo = tmp_path / "feedback.pipe"
    send_fifo = tmp_path / "feedback.pipe.send"
    lock = tmp_path / "feedback.pipe.lock"
    with komtar_process(fifo, "serve", str(ROOT / "tests/fixtures/site")) as (
        _server,
        process,
    ):
        for path in (fifo, send_fifo, lock):
            assert path.exists()
            assert path.stat().st_mode & 0o777 == 0o600
        fifo.unlink()
        process.wait(timeout=5)
        assert process.returncode != 0

    assert process.stdout is not None
    output = process.stdout.read()
    assert "receive feedback with: komtar recv --fifo" in output
    assert "was deleted or replaced" in output
    assert not send_fifo.exists()
    assert not lock.exists()

    with komtar_process(fifo, "serve", str(ROOT / "tests/fixtures/site")) as (
        _server,
        process,
    ):
        fifo.unlink()
        fifo.write_text("replacement owned by the user", encoding="utf-8")
        process.wait(timeout=5)
        assert process.returncode != 0

    assert fifo.read_text(encoding="utf-8") == "replacement owned by the user"
    assert not send_fifo.exists()
    assert not lock.exists()


def test_recv_ignores_empty_sessions_and_returns_each_batch_unchanged(
    tmp_path: Path,
) -> None:
    fifo = tmp_path / "feedback.pipe"
    command = [str(TARGET_DIR / "debug/komtar"), "recv", "--fifo", str(fifo)]
    first = subprocess.Popen(
        command,
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    deadline = time.monotonic() + 5
    while not fifo.exists() and time.monotonic() < deadline:
        time.sleep(0.01)
    assert fifo.exists()
    with fifo.open("wb"):
        pass
    time.sleep(0.1)
    assert first.poll() is None

    first_batch = (
        b'{"version":1,"comment":"first","target":{"selector":"#intro"}}\n'
        b'{"version":1,"comment":"second","extra":true}\n'
    )
    with fifo.open("wb", buffering=0) as writer:
        _ = writer.write(first_batch)
    stdout, stderr = first.communicate(timeout=5)
    assert first.returncode == 0, stderr.decode()
    assert stdout == first_batch

    second = subprocess.Popen(
        command,
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    second_batch = b'{"version":1,"comment":"after edits"}\n'
    with fifo.open("wb", buffering=0) as writer:
        _ = writer.write(second_batch)
    stdout, stderr = second.communicate(timeout=5)
    assert second.returncode == 0, stderr.decode()
    assert stdout == second_batch

    third = subprocess.Popen(
        command,
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    complete_before_partial = b'{"version":1,"comment":"complete"}\npartial'
    with fifo.open("wb", buffering=0) as writer:
        _ = writer.write(complete_before_partial)
    stdout, stderr = third.communicate(timeout=5)
    assert third.returncode == 0, stderr.decode()
    assert stdout == b'{"version":1,"comment":"complete"}\n'

    fourth = subprocess.Popen(
        command,
        cwd=ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    with fifo.open("wb", buffering=0) as writer:
        _ = writer.write(b"partial")
    time.sleep(0.1)
    assert fourth.poll() is None
    with fifo.open("wb", buffering=0) as writer:
        _ = writer.write(b" remainder\n")
    stdout, stderr = fourth.communicate(timeout=5)
    assert fourth.returncode == 0, stderr.decode()
    assert stdout == b"partial remainder\n"


@pytest.mark.parametrize("shutdown", [signal.SIGINT, signal.SIGHUP, signal.SIGQUIT])
def test_shutdown_signals_remove_owned_transport(tmp_path: Path, shutdown: int) -> None:
    fifo = tmp_path / "feedback.pipe"
    paths = (fifo, Path(f"{fifo}.send"), Path(f"{fifo}.lock"))
    with komtar_process(fifo, "serve", str(ROOT / "tests/fixtures/site")) as (
        _server,
        process,
    ):
        process.send_signal(shutdown)
        process.wait(timeout=5)
        assert process.returncode == 0
    assert all(not path.exists() for path in paths)


def test_comment_endpoint_rejects_invalid_requests(tmp_path: Path) -> None:
    fifo = tmp_path / "feedback.pipe"
    with komtar(fifo, "serve", str(ROOT / "tests/fixtures/site")) as server:
        endpoint = f"{server}/_komtar/api/comments"
        cases = (
            (b"{}", "text/plain", 415),
            (b"not json", "application/json", 400),
            (b"x" * (64 * 1024 + 1), "application/json", 413),
        )
        for body, content_type, expected in cases:
            request = Request(
                endpoint,
                data=body,
                headers={"Content-Type": content_type},
                method="POST",
            )
            with pytest.raises(HTTPError) as failure:
                _ = urlopen(request, timeout=5)
            assert failure.value.code == expected

        address = server.removeprefix("http://")
        host, port_text = address.rsplit(":", maxsplit=1)
        connection = http.client.HTTPConnection(host, int(port_text), timeout=5)
        try:
            connection.request(
                "POST",
                "/_komtar/api/comments",
                body=iter([b"x" * (64 * 1024 + 1)]),
                headers={"Content-Type": "application/json"},
                encode_chunked=True,
            )
            response = connection.getresponse()
            assert response.status == 413
            _ = response.read()
        finally:
            connection.close()


def test_send_validation_direct_input_recovery_and_offline_failure(page: Page) -> None:
    with tempfile.TemporaryDirectory() as temporary, upstream_server() as upstream:
        fifo = Path(temporary) / "comments.fifo"
        send_fifo = Path(f"{fifo}.send")
        with komtar(fifo, upstream) as proxy:
            page.goto(proxy, wait_until="networkidle")

            positional = run_cli(
                "send", "not-a-message-argument", "--fifo", str(fifo), input_text="body"
            )
            assert positional.returncode != 0
            assert (
                run_cli("send", "--fifo", str(fifo), input_text=" \n").returncode != 0
            )
            assert (
                run_cli(
                    "send",
                    "--fifo",
                    str(fifo),
                    "--anchor",
                    " ",
                    input_text="answer",
                ).returncode
                != 0
            )

            send_message(fifo, "Valid **Markdown** answer")
            expect(page.locator("#komtar .agent-message")).to_contain_text(
                "Valid Markdown answer"
            )

            valid = b'{"version":1,"message":"Recovered after malformed input"}\n'
            raw = (
                b"not-json\n"
                b'{"version":2,"message":"wrong version"}\n'
                b'{"version":1,"message":"unknown field","extra":true}\n'
                + b"x" * (128 * 1024 + 1)
                + b"\n"
                + valid
            )
            with send_fifo.open("wb", buffering=0) as writer:
                _ = writer.write(raw)
            expect(
                page.locator(
                    "#komtar .agent-message", has_text="Recovered after malformed input"
                )
            ).to_be_visible()

        assert not fifo.exists()
        assert not send_fifo.exists()
        offline = run_cli("send", "--fifo", str(fifo), input_text="No server")
        assert offline.returncode != 0
        assert "no Komtar server is reading" in offline.stderr


def test_agent_messages_broadcast_replay_markdown_and_tab_local_dismissal(
    browser: Browser,
) -> None:
    with tempfile.TemporaryDirectory() as temporary, upstream_server() as upstream:
        fifo = Path(temporary) / "comments.fifo"
        with komtar(fifo, "proxy", upstream) as proxy:
            first = browser.new_page()
            second = browser.new_page()
            late = browser.new_page()
            try:
                first.goto(proxy, wait_until="networkidle")
                second.goto(proxy, wait_until="networkidle")
                send_message(
                    fifo,
                    "Items:\n\n- first\n- second\n\n"
                    "<script>window.komtarInjected = true</script>\n\n"
                    "[Documentation](https://example.com/docs)",
                )
                for open_page in (first, second):
                    card = open_page.locator("#komtar .agent-message")
                    expect(card).to_contain_text("first")
                    expect(card.locator("li")).to_have_count(2)
                    expect(card.locator("script")).to_have_count(0)
                    expect(card).to_contain_text("<script>")
                    link = card.get_by_role("link", name="Documentation")
                    expect(link).to_have_attribute("target", "_blank")
                    expect(link).to_have_attribute("rel", "noopener noreferrer")
                    assert open_page.evaluate("window.komtarInjected") is None

                late.goto(proxy, wait_until="networkidle")
                expect(late.locator("#komtar .agent-message")).to_contain_text("Items:")

                first.get_by_role("button", name="Dismiss agent message").click()
                expect(first.locator("#komtar .agent-message")).to_have_count(0)
                expect(second.locator("#komtar .agent-message")).to_have_count(1)
                first.reload(wait_until="networkidle")
                expect(first.locator("#komtar .agent-message")).to_have_count(0)
                expect(second.locator("#komtar .agent-message")).to_have_count(1)
            finally:
                first.close()
                second.close()
                late.close()


def test_global_shortcut_and_comments_on_agent_responses(page: Page) -> None:
    with tempfile.TemporaryDirectory() as temporary, upstream_server() as upstream:
        fifo = Path(temporary) / "comments.fifo"
        with komtar(fifo, upstream) as proxy:
            page.goto(proxy, wait_until="networkidle")
            dialog = page.locator("#komtar-dialog")

            _ = page.evaluate(
                """() => {
                  const input = document.createElement('input');
                  input.id = 'shortcut-input';
                  document.body.append(input);
                }"""
            )
            editable = page.locator("#shortcut-input")
            editable.focus()
            editable.press("/")
            expect(editable).to_have_value("/")
            expect(dialog).to_be_hidden()
            editable.blur()

            page.keyboard.press("/")
            expect(dialog).to_be_visible()
            expect(page.locator("#komtar #dialog-label")).to_have_text("Comment:")
            expect(page.locator("#komtar #selection-preview")).to_be_hidden()
            page.locator("#komtar textarea").fill("A page-level question")
            page.locator("#komtar textarea").press("Enter")
            expect(dialog).to_be_hidden()

            delivered = run_cli("recv", "--fifo", str(fifo))
            assert delivered.returncode == 0, delivered.stderr
            page_record = json.loads(delivered.stdout)
            assert page_record["comment"] == "A page-level question"
            assert page_record["target"]["selector"] == ""
            assert page_record["target"]["selectedText"] is None
            assert page_record["pointer"]["targetSize"] == {
                "width": 0,
                "height": 0,
            }

            send_message(fifo, "Initial agent response")
            initial = page.locator(
                "#komtar .agent-message", has_text="Initial agent response"
            )
            expect(initial).to_be_visible()
            message_id = initial.get_attribute("data-message-id")
            assert message_id is not None
            initial.click(button="right", position={"x": 20, "y": 20})
            expect(dialog).to_be_visible()
            expect(page.locator("#komtar #dialog-label")).to_have_text(
                "Comment on agent response:"
            )
            page.locator("#komtar textarea").fill("Can you clarify this response?")
            page.get_by_role("button", name="Queue comment").click()
            expect(dialog).to_be_hidden()

            delivered = run_cli("recv", "--fifo", str(fifo))
            assert delivered.returncode == 0, delivered.stderr
            response_record = json.loads(delivered.stdout)
            response_selector = f"komtar-agent:{message_id}"
            assert response_record["comment"] == "Can you clarify this response?"
            assert response_record["target"]["selector"] == response_selector
            assert "Initial agent response" in response_record["target"]["text"]

            send_message(
                fifo, "Clarification attached to the response", response_selector
            )
            clarification = page.locator(
                "#komtar .agent-message", has_text="Clarification attached"
            )
            expect(clarification).to_be_visible()
            assert (
                clarification.evaluate("node => node.parentElement.id")
                == "agent-anchored"
            )
            expect(clarification.get_by_text("Target unavailable")).to_be_hidden()
            initial_box = initial.bounding_box()
            clarification_box = clarification.bounding_box()
            assert initial_box is not None and clarification_box is not None
            separated = (
                clarification_box["x"] + clarification_box["width"] <= initial_box["x"]
                or clarification_box["x"] >= initial_box["x"] + initial_box["width"]
            )
            assert separated


def test_anchored_stacking_missing_anchor_reattachment_and_element_links(
    page: Page,
) -> None:
    with tempfile.TemporaryDirectory() as temporary, upstream_server() as upstream:
        fifo = Path(temporary) / "comments.fifo"
        with komtar(fifo, upstream) as proxy:
            page.goto(proxy, wait_until="networkidle")
            _ = page.evaluate(
                """() => {
                  const spacer = document.createElement('div');
                  spacer.style.height = '1400px';
                  const complex = document.createElement('button');
                  complex.id = 'complex';
                  complex.dataset.kind = 'a b';
                  complex.textContent = 'Complex target';
                  document.body.append(spacer, complex);
                }"""
            )

            send_message(fifo, "First anchored answer", "#intro")
            send_message(fifo, "Second anchored answer", "#intro")
            first = page.locator("#komtar .agent-message", has_text="First anchored")
            second = page.locator("#komtar .agent-message", has_text="Second anchored")
            expect(first).to_be_visible()
            expect(second).to_be_visible()
            assert first.evaluate("node => node.parentElement.id") == "agent-anchored"
            assert second.evaluate("node => node.parentElement.id") == "agent-anchored"
            first_box = first.bounding_box()
            second_box = second.bounding_box()
            assert first_box is not None and second_box is not None
            assert second_box["y"] > first_box["y"]
            assert 0 <= first_box["x"] <= 1280 - first_box["width"]
            first.locator(".agent-dismiss").focus()
            _ = page.evaluate("window.dispatchEvent(new Event('resize'))")
            assert first.evaluate(
                "node => node.querySelector('.agent-dismiss') === node.getRootNode().activeElement"
            )

            send_message(fifo, "Waiting for a later element", "#later")
            missing = page.locator("#komtar .agent-message", has_text="Waiting for")
            expect(missing.get_by_text("Target unavailable")).to_be_visible()
            assert missing.evaluate("node => node.parentElement.id") == "agent-general"
            _ = page.evaluate(
                """() => {
                  const later = document.createElement('aside');
                  later.id = 'later';
                  later.textContent = 'Now available';
                  document.querySelector('main').append(later);
                }"""
            )
            page.wait_for_function(
                """() => {
                  const host = document.querySelector('#komtar');
                  const card = [...host.shadowRoot.querySelectorAll('.agent-message')]
                    .find((node) => node.textContent.includes('Waiting for'));
                  return card?.parentElement?.id === 'agent-anchored';
                }"""
            )
            expect(missing.get_by_text("Target unavailable")).to_be_hidden()

            send_message(
                fifo,
                "[Jump to the complex target]"
                "(komtar:%23complex%5Bdata-kind%3D%22a%20b%22%5D)",
            )
            page.get_by_role("link", name="Jump to the complex target").click()
            expect(page.locator("#komtar #message-highlight")).to_be_visible()
            in_view = page.locator("#complex").evaluate(
                "node => { const rect = node.getBoundingClientRect(); "
                "return rect.top >= 0 && rect.bottom <= innerHeight; }"
            )
            assert in_view is True

            send_message(fifo, "Invalid anchor answer", "[")
            invalid_anchor = page.locator(
                "#komtar .agent-message", has_text="Invalid anchor answer"
            )
            expect(invalid_anchor.get_by_text("Target unavailable")).to_be_visible()
            assert (
                invalid_anchor.evaluate("node => node.parentElement.id")
                == "agent-general"
            )

            send_message(
                fifo,
                "[Invalid element link](komtar:%E0%A4%A) "
                "[Missing element](komtar:%23does-not-exist)",
            )
            page.get_by_role("link", name="Invalid element link").click()
            expect(page.locator("#komtar #toast")).to_have_text(
                "Element link is invalid"
            )
            page.get_by_role("link", name="Missing element").click()
            expect(page.locator("#komtar #toast")).to_have_text(
                "Element is unavailable"
            )


def test_static_messages_and_custom_transport_paths_are_hidden_and_ignored(
    page: Page, tmp_path: Path
) -> None:
    site = tmp_path / "site"
    site.mkdir()
    index = site / "index.html"
    index.write_text('<body><h1 id="title">Static title</h1></body>', encoding="utf-8")
    fifo = site / "feedback.pipe"
    with komtar(fifo, "serve", str(site)) as server:
        page.add_init_script(
            """sessionStorage.setItem(
              'komtar-static-loads',
              String(Number(sessionStorage.getItem('komtar-static-loads') || '0') + 1),
            );"""
        )
        page.goto(server, wait_until="networkidle")
        for name in ("feedback.pipe", "feedback.pipe.send", "feedback.pipe.lock"):
            with pytest.raises(HTTPError) as hidden:
                urlopen(f"{server}/{name}", timeout=2)
            assert hidden.value.code == 404

        # Let any backend events from constructing the fresh fixture settle, then
        # measure reloads caused specifically by transport activity.
        time.sleep(0.5)
        _ = page.evaluate("sessionStorage.setItem('komtar-static-loads', '0')")
        send_message(fifo, "Static mode answer", "#title")
        expect(page.locator("#komtar .agent-message")).to_contain_text(
            "Static mode answer"
        )
        time.sleep(0.5)
        assert page.evaluate("sessionStorage.getItem('komtar-static-loads')") == "0"

    assert not fifo.exists()
    assert not Path(f"{fifo}.send").exists()
    assert not Path(f"{fifo}.lock").exists()
