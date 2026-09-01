set shell := ["sh", "-cu"]

fmt:
    cargo fmt --check

lint:
    cargo clippy --all-targets --all-features -- -D warnings

lint-js:
    npm run lint

fmt-python:
    uv run ruff format --check .

lint-python:
    uv run ruff check .

typecheck-python:
    uv run basedpyright

test:
    cargo test

e2e:
    uv run pytest

build:
    cargo build --release

ci: fmt lint lint-js fmt-python lint-python typecheck-python test e2e build
