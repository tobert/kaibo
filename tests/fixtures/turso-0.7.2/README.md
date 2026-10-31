# turso 0.7.2 state db fixture

A `state.db` with its live `-wal` and `-tshm` sidecars, written by kaibo's real
`SessionStore` under the `turso =0.7.2` pin. The files were copied while the store was
still open, so the WAL is uncheckpointed, as a killed MCP server leaves it.

Content: session `fixture-session` (2 turns), session `other` (1 turn), and one batch
handle `anthropic/msgbatch_fixture` (label `fixture-label`, submitted_count 7).

`tests/store.rs::opens_a_state_db_written_by_turso_0_7_2` copies these files to a temp
dir and requires the current pin to open, read, and write them. Regenerate only under the
0.7.2 pin: `git checkout 280ef98 -- Cargo.toml Cargo.lock`, then
`KAIBO_FIXTURE_OUT=$PWD/tests/fixtures/turso-0.7.2 cargo test --test store -- --ignored generate_turso_fixture`.
