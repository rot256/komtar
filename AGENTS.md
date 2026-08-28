# Repository instructions

`komtar` is a development-only Rust reverse proxy. Keep the runtime as one
self-contained binary: JavaScript and CSS belong in `src/client.js` and are
embedded with `include_str!`.

Run the checks below before handing off changes:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
uv run pytest
cargo build --release
```

Do not add production integration hooks. A project is annotated only when its
developer deliberately visits the `komtar proxy` address or adds the documented
development-only script tag. Preserve the version-1 newline-delimited JSON schema
unless a breaking format change is explicitly requested.
