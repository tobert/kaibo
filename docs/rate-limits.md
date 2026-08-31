# Outbound rate limiting and transport retry — plan

Living plan, `docs/probes.md` style: milestones up top, status log at the bottom.
The branch is `rate-limits`; the worktree is `~/src/wt/kaibo-rate-limits`.

## The problem, measured

- 2026-08-27: two `deliberate` calls on cast `gpt-deliberate` died in the DOSSIER
  phase. The explorer (`gpt-5.6-luna`) hit `429 tokens per min (TPM): Limit
  200000` on three attached files, kaibo neither backed off nor retried, and the
  whole investigation was lost before the synth ran.
- kaibo sends bursts by construction: rig runs a turn's tool calls with
  `buffer_unordered`, and a consult can fan out sweeps.
- Non-goal, stated so nobody reaches for this plan to fix it: the 2026-08 OpenAI
  batch `validating` failure ("Cannot find file …") is provider-side, reproduced
  with plain curl and reported by many others since 2026-08-19. Retry at kaibo's
  layer does not address it.

## Findings that shape the design (verified against vendored rig 0.41)

- rig-core ships `RetryPolicy`/`ExponentialBackoff` (`http_client/retry.rs`) but
  wires it only into the SSE event source — streaming reconnection. kaibo's
  non-streaming prompt loop never touches it.
- rig-agent's retry is `ModelTurnAction::Retry` from `AgentHook`s: it re-rolls a
  *completed* model turn. A transport error exits the run as `Err` without
  reaching any hook.
- `CompletionError::provider_response_status()` recovers the HTTP status on the
  error path; `provider_response_body()` the body. Headers are dropped
  (`http_client/mod.rs::non_success_status_error` keeps status + body text), so
  `Retry-After` is unrecoverable on the completions path. OpenAI usually embeds
  "try again in Xs" in the 429 body; Anthropic's delay lives only in the lost
  header.
- kaibo already owns the seam: `src/completion_retry.rs` (`Retried<M>`) retries
  malformed generations underneath the loop, wired as `watched(retried(model))`
  at both phase call sites (`src/consult/engine.rs:1247`, `:1682`). Its module
  doc says transport retry "would take an upstream contribution to rig itself" —
  superseded: `provider_response_status()` gives the wrapper everything it needs.
  M1 updates that sentence.

## Design

Three pieces, in priority order. The first two are this branch; the third is a
recorded decision to defer.

### M1 — transient transport retry, at the `Retried` seam

A second failure class beside malformed-generation, in the same wrapper (one
module owns "kaibo sent a provider request twice"):

- **Classification.** Transient transport = status 429, 500, 502, 503, 529
  (Anthropic overloaded) from `provider_response_status()`. When the status is
  absent (a provider string that never carried one), fall back to conservative
  text markers, the same posture `is_malformed_generation` documents. Rejected
  requests (400/401/403/404/422) and parse errors keep their current behavior:
  one attempt, error up.
- **Waiting.** Exponential backoff with full jitter; best-effort delay parse
  from the body when the provider embeds one ("try again in 1.2s"). Bounds are
  constants first (like `MALFORMED_RETRIES`): start 1s, factor 2, per-wait cap
  60s, 4 further attempts. Config keys only on evidence that one deployment
  needs different numbers.
- **Timeouts stay out**, initially. A request that timed out may be a request
  that is too large; retrying it doubles the spend to fail twice. Add on
  evidence from a live probe, the same rule the malformed-marker neighbors
  follow.
- **Loud.** One `tracing::warn!` per wait: model, status, attempt, delay. The
  count per model is the observability payoff.
- **Tests, failing first.** Scripted client (`test_support.rs`) responders that
  429 then answer; `#[tokio::test(start_paused = true)]` so backoff is asserted
  deterministically (elapsed virtual time, attempt count); a bounded-failure
  test proving the provider's own words survive; a rejected-request test proving
  a 400 still costs exactly one request.

### M2 — proactive per-backend limiter

- `governor` (pure Rust) keyed by backend name, one process-wide registry so
  every arm on a backend shares the budget.
- Config: `requests_per_minute` under `[backends.<name>]`; absent = no limit
  (today's behavior). Template line in `docs/config.example.toml`, semantics in
  `docs/config.md`.
- Acquire before **every** attempt, retries included — a retry is a request.
- No client-side TPM enforcement: kaibo can only estimate tokens, and a wrong
  estimate silently throttles or silently overruns. Requests per minute is the
  honest knob; the 429 backoff above absorbs what it cannot express.
- Dependency discipline before merge: `cargo tree -i aws-lc-rs` and
  `-i mimalloc` empty, and no `cc`/cmake build script enters the graph.

### M3 — deferred: `Retry-After` on kaibo-owned reqwest surfaces

`batch.rs` and the media clients hold raw `reqwest::Response`s, so exact
`Retry-After` honoring is possible there — but no failure has been observed on
those paths, batch polls are cheap, and the batch lane's real problem is
provider-side. Recorded so the option is known; built when a failure earns it.

## Status log

- 2026-08-31 — plan written from the session that diagnosed the 08-27 429 and
  confirmed rig 0.41 offers nothing on the non-streaming path. M1 next.
