# turso 0.7.2 state db fixture

A `state.db` with its live `-wal` and `-tshm` sidecars, written by kaibo's real
`SessionStore` under the `turso =0.7.2` pin at `SCHEMA_VERSION` 2. The files were copied
while the store was still open, so the WAL is uncheckpointed, as a killed MCP server
leaves it.

Content: session `fixture-session` (2 turns), session `other` (1 turn), and batch handle
`anthropic/msgbatch_fixture` (label `fixture-label`, submitted_count 7, `created_at`
4102444800 = year 2100, so the 30-day TTL prune at open can never remove it).

`tests/store.rs::opens_a_state_db_written_by_turso_0_7_2` copies these files to a temp dir
and requires the current pin to open, read, and write them. It first asserts the
premise: `state.db` is exactly 4096 bytes and `state.db-wal` is non-empty.

## Regenerating

WARNING: run the generator only under the 0.7.2 writer. Under any newer pin it silently
turns this into a newer-version fixture and the compatibility test proves nothing. The
schema must stay at `SCHEMA_VERSION` 2.

1. `git checkout 280ef98 -- Cargo.toml Cargo.lock src/store.rs`
2. `KAIBO_FIXTURE_OUT=$PWD/tests/fixtures/turso-0.7.2 cargo test --locked --test store -- --ignored generate_turso_fixture`
3. Verify `cargo tree -i turso --locked` says `turso v0.7.2`, then the sizes and sums below.
4. Restore: `git checkout HEAD -- Cargo.toml Cargo.lock src/store.rs`

## Current files

| file | bytes | sha256 |
|---|---|---|
| `state.db` | 4096 | `2bcfad6941602bb75efb40a0f81a310d59075336378cf4cf9bc0bb0522629134` |
| `state.db-tshm` | 86016 | `42b05e1eb26e996b1fc7a1fa260f2712029aab82a1c9e922f0098f9bcba76d3c` |
| `state.db-wal` | 123632 | `9f6169a5541561775ec5523c855a10ba1f26169905aad89aa9953c5b643ddf3c` |
