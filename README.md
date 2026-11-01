# ryo-v2

Speculative refactoring engine for Rust: Blueprints explored and verified in-process on rust-analyzer, built once after commit

What it does and why, as the concept every design decision rests on: [CONCEPT.md](CONCEPT.md).
How development is done: [DEVELOPMENT.md](DEVELOPMENT.md); basic conventions:
[CONTRIBUTING.md](CONTRIBUTING.md).

## Crates

| crate | role |
|---|---|
| `ryo-v2` | Umbrella: the name users depend on. Re-exports the core. |
| `ryo-v2-core` | Core (SDK): `domain` (pure rules and types), `app` (use cases and their ports), `infra` (adapters with real I/O). |
| `ryo-v2-ra` | Adapter: rust-analyzer behind the core's ports — the in-memory code state, forks and diagnostics. |
| `ryo-v2-host` | Host: the `ryo-v2` binary, which runs the core behind a front such as the command line. |
| `ryo-v2-oracle` | Measurement, not published: the oracle and search-recall harnesses that judge the verifier and the search against `cargo check` and the planted corpus. |
| `ryo-v2-ledger-merge` | Tooling, not published: the merge driver of the ledgers, by section and table row. |

The layer rules are in `arch-lint.toml` and run with `cargo test`. A crate or layer
that does not fit the project is deleted, together with its `arch-lint.toml` scope.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
