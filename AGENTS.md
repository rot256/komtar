# Repository instructions

`komtar` is a development-only Rust reverse proxy. Keep the runtime as one
self-contained binary: JavaScript and CSS belong in `assets/client.js` and are
embedded with `include_str!`.

Run the checks below before handing off changes:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
npm ci
npm run lint
uv run pytest
cargo build --release
```

Do not add production integration hooks. A project is annotated only when its
developer deliberately visits the address printed by `komtar`. Preserve the
version-1 newline-delimited JSON schema unless a breaking format change is
explicitly requested.
