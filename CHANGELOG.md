# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

The crate has not yet had a tagged release; everything below is pre-`0.1.0` work.

### Added

- Library-first lifecycle: `Coalition<T>` exposes `cancellation_token()` and
  `shutdown().await` so downstream binaries wire their own top-level shutdown.
- Readiness handshake in `Coalition::new` — awaits every agent's LIVE-query
  registration before returning, guaranteeing the first `Agent::send` is observed.
- Generic `Message<T: SurrealValue>` edge record over user-typed payloads.
- Panic-safe integration-test teardown via `tokio-util` `DropGuard`.
- GitHub Actions CI: `fmt --check`, `clippy -D warnings`, Docker integration test.
- Pinned toolchain (`rust-toolchain.toml`, 1.96.0 + rustfmt/clippy).
- `CLAUDE.md` documenting test workflow, SDK gotchas, and the shutdown contract.

### Changed

- Bumped `surrealdb`/`surrealdb-types` to 3.1.3 and `bollard` to 0.21.

### Fixed

- Edge fields now use `#[surreal(rename = "in")]` — serde `rename` is ignored by
  the `SurrealValue` derive, so `r#in` would otherwise serialize as `"r#in"`.
- LIVE subscription projects edge pointers explicitly
  (`LIVE SELECT *, in, out FROM message ...`); plain `LIVE SELECT *` omits `in`/`out`.
- Subscription race resolved by the readiness handshake (see Added).
