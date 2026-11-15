# ryo-v2

Speculative refactoring engine for Rust: Blueprints explored and verified in-process on rust-analyzer, built once after commit

What it does and why, as the concept every design decision rests on: [CONCEPT.md](CONCEPT.md).
How development is done: [DEVELOPMENT.md](DEVELOPMENT.md); basic conventions:
[CONTRIBUTING.md](CONTRIBUTING.md).

## Status

Nothing is released and no input or output format is stable. [CONCEPT.md](CONCEPT.md) is the
target the code is brought to, not a description of what runs today. Today the `ryo-v2` command
line has the commands `bench` (timing and memory of the verifier), `apply` (verify a set of
Blueprints given as JSON, a dry run), `search` (search the best verified changes of one decision
from a request JSON of targets, probes, roots, budget and judge, a dry run) and `fingerprint`
(prints the fingerprints a Blueprint cites). The search is a best-first beam search on N live
forks of one loaded rust-analyzer database (the core's `Forks` port), with judges that need no
model. Not yet built: TypeSpec input, Monte Carlo search, a model-backed judge and a verdict
cache.

What CONCEPT.md calls a command is held in a `Blueprint` in the code, which adds the text
splices, the ranges it read and a decision record; a mutation list is a set of Blueprints (the
glossary is in `crates/ryo-v2-core/src/domain/mod.rs`).

## Crates

| crate | role |
|---|---|
| `ryo-v2` | Umbrella: the name users depend on. Re-exports the core. |
| `ryo-v2-core` | Core (SDK): `domain` (pure rules and types), `app` (use cases and their ports), `infra` (adapters with real I/O). |
| `ryo-v2-ra` | Adapter: rust-analyzer behind the core's ports — the in-memory code state, forks and diagnostics. |
| `ryo-v2-engine` | The fork Engine, not published: layers of writes over one frozen analysed salsa database, several live at once, bound to rust-analyzer's database. |
| `ryo-v2-host` | Host: the `ryo-v2` binary, which runs the core behind a front such as the command line, and the `engine-probe` binary, which measures the fork Engine (memory per layer, depth). |
| `ryo-v2-oracle` | Measurement, not published: the oracle and search-recall harnesses that judge the verifier and the search against `cargo check` and the planted corpus. |
| `ryo-v2-ledger-merge` | Tooling, not published: the merge driver of the ledgers, by section and table row. |

The layer rules are in `arch-lint.toml` and run with `cargo test`. A crate or layer
that does not fit the project is deleted, together with its `arch-lint.toml` scope.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
