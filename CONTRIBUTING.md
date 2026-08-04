# Contributing to turboGP

## Getting started

1. Clone the repo
2. Install Rust 1.97+ (`rustup install stable`)
3. `cargo build` — must compile clean
4. `cargo test --lib --tests` — all 1331+ tests must pass (1048 lib + 283 integration)
5. `cargo fmt --check` — must pass
6. `cargo clippy` — should pass (warnings are tolerated; `-D warnings` is aspirational)

## Branch naming

- `feat/<short-description>` — new features
- `fix/<short-description>` — bug fixes
- `docs/<short-description>` — documentation only
- `kernel/<short-description>` — kernel table changes
- `adr/<number>-<short-description>` — ADR implementation

## Commit format

```
<type>: <description>

<body>
```

Types: `feat`, `fix`, `docs`, `refactor`, `test`, `kernel`, `adr`

## Pull request process

1. Create a branch from `main`
2. Write code + tests
3. Run `cargo fmt && cargo clippy -- -D warnings && cargo test`
4. Open a PR using the template
5. All CI checks must pass
6. One review required for merge

## Code style

- Follow `rustfmt.toml` (run `cargo fmt` before committing)
- No `unwrap()` or `expect()` in production code — use `?` and `Result`
- All `unsafe` blocks must have a `// SAFETY:` comment explaining the invariant
- All public functions must have doc comments
- All new kernels must be benchmarked (add to `benches/`)

## Architecture

Before contributing, read:
1. [FINE_DRAFT.md](docs/FINE_DRAFT.md) — the venture and architecture
2. [SPECIFICATION.md](SPECIFICATION.md) — formal interface specification
3. [docs/adr/](docs/adr/) — accepted design decisions

New work should trace to a problem in [docs/problems/](docs/problems/) and an
ADR in [docs/adr/](docs/adr/). If no ADR exists, write one first.

## Testing

- Unit tests: `#[test]` in each module (currently 66 tests)
- Integration tests: `tests/` directory (end-to-end)
- Benchmarks: `benches/` directory (criterion)

Every new kernel MUST have:
1. A correctness test (does it produce the right answer?)
2. A parity test (does AVX-512 match scalar?)
3. A benchmark (what's the throughput?)

## License

By contributing, you agree your contributions are licensed under the
[CCL-X License](LICENSE.md).
