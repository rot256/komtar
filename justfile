set shell := ["sh", "-cu"]

fmt:
    cargo fmt --check

lint:
    cargo clippy --all-targets --all-features -- -D warnings

test:
    cargo test

e2e:
    uv run pytest

build:
    cargo build --release

ci: fmt lint test e2e build
