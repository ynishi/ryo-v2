# ryo-v2

Speculative refactoring engine for Rust: Blueprints explored and verified in-process on rust-analyzer, built once after commit

## Crates

| crate | role |
|---|---|
| `ryo-v2` | Umbrella: the name users depend on. Re-exports the core. |
| `ryo-v2-core` | Core (SDK): `domain` (pure rules and types), `app` (use cases and their ports), `infra` (adapters with real I/O). |
| `ryo-v2-cli` | The `ryo-v2` command. |

The layer rules are in `arch-lint.toml` and run with `cargo test`. A crate or layer
that does not fit the project is deleted, together with its `arch-lint.toml` scope.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
