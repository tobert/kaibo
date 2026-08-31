# BFL (FLUX) media kind — spec

Living spec for the `bfl` media kind: Black Forest Labs' FLUX image API behind
`generate`, the same facade every other media kind speaks. Branch `bfl`,
worktree `~/src/wt/kaibo-bfl`. Written 2026-08-31 from the live OpenAPI spec at
`https://api.bfl.ai/openapi.json` (snapshot in the drafting session's
scratchpad; re-fetch when in doubt — the spec is the ground truth, this doc is
the map).

## Why

FLUX covers the aesthetic ground Midjourney owns, and Midjourney has no lawful
API (their ToS bans automated access; the third-party wrappers puppet Discord
accounts). BFL's own API is keyed, documented, and one-PR-shaped for kaibo's
media lane. The motivating use is concept images.

## The wire

- Base `https://api.bfl.ai`, auth header `x-key: <api key>`, JSON request
  bodies (not multipart — unlike Stability).
- One endpoint per operation, like Stability: the `op` call parameter maps to a
  path (`POST /v1/flux-2-pro`, `POST /v1/flux-kontext-pro`, …).
- **Every operation is asynchronous.** A create returns
  `AsyncResponse{id, polling_url, cost, input_mp, output_mp}`. Poll the
  returned `polling_url` verbatim (regional host — never reconstruct it from
  the base URL) until `ResultResponse.status` leaves
  `Pending`/`Reasoning`/`Generating`.
- **The poll leg is https-only with no redirects.** `polling_url` is an
  address BFL chose, not the operator's own `base_url`, and the poll request
  carries the `x-key` credential — the same trust shape
  `crate::cas::fetch_artifact_bytes` already takes for `result.sample`, and
  worse, since the key rides along. A plain `http://` `polling_url` is
  refused before a single byte of the request leaves (no connection is
  attempted at all — `crate::tls::artifact_fetch_client`'s `https_only(true)`),
  and a redirecting `polling_url` is refused rather than followed
  (`Policy::none()`) — reqwest strips the standard `Authorization` header on a
  cross-host redirect but not a custom header like `x-key`, so following one
  would carry the key to whatever host it names. The create call itself keeps
  the general client: `base_url` is the operator's own configured endpoint,
  and where that redirects is the operator's business.
- Terminal statuses: `Ready` (the artifact), `Error`, `Request Moderated`,
  `Content Moderated`, `Task not found` — each of the last four is its own loud
  error naming what the provider said and what the caller can change.
- On `Ready`, `result.sample` is a **signed delivery URL that expires in ~10
  minutes** — fetch into the media CAS immediately on observing `Ready`, via
  `crate::tls::artifact_fetch_client` (one hop, https-only; the DashScope
  precedent). A `3xx` or expired link is its own error, not a generic failure.

## Mapping to the facade

- `MediaRequest.prompt` → `prompt` (required everywhere).
- `MediaRequest.inputs` (named, from #164) → the `input_image` …
  `input_image_8` request fields, base64-encoded strings in the JSON body. The
  name in `inputs` is the field name verbatim.
- `MediaRequest.fields` → passthrough scalars (`width`, `height`, `seed`,
  `safety_tolerance`, `output_format`, `disable_pup`, …), the #166 pattern:
  seed nothing the caller did not say, refuse unknown fields loudly if that is
  what the existing kinds do — mirror them.
- Cost: `AsyncResponse.cost` is credits, reported by the provider **per
  request**. Publish it verbatim in the outcome/provenance (native-unit ruling,
  2026-08-22: kaibo converts nothing). No static per-op cost table — the
  response is the table.

## Deferred vs. complete

Mirror the existing deferred-operation handling (Stability's five deferred ops
are the precedent — read how `stability.rs` and the `generate` face decide
between `MediaOutcome::Complete` and `Deferred` and do exactly that). If the
existing shape polls inline within the request timeout for fast ops, use it —
FLUX generations are seconds-fast and a concept-image caller wants bytes, not a
handle. If the facade's shape is strictly Deferred-with-job-N for async
providers, use that. State which shape resulted in the report; do not invent a
third shape.

## Starter op table

Five ops, the subset discipline from #166. Path, purpose, one line each in the
`op` schema doc:

| op | path | why in the starter set |
|---|---|---|
| `flux-dev` | `/v1/flux-dev` | the cheap rung — probing and drafts |
| `flux-2-pro` | `/v1/flux-2-pro` | the quality default |
| `flux-2-flex` | `/v1/flux-2-flex` | parameter-heavy control |
| `flux-pro-1.1-ultra` | `/v1/flux-pro-1.1-ultra` | high-resolution stills |
| `flux-kontext-pro` | `/v1/flux-kontext-pro` | image editing with reference inputs |

## Config

`[backends.bfl]` with `kind = "bfl"`, key sourced the way the other media
backends source theirs (mirror `stability`/`dashscope` exactly), plus whatever
cast/slot wiring `generate` needs to staff the kind — #168 (Gemini images) is
the freshest merged example of adding a kind end to end; use it as the
template.

## Out of scope, on record

- `flux-3-video` and `flux-tools/video-upscale-v1` — blocked on the CAS
  extension gap (no video extensions in `to_cas_extension`; it refuses loudly
  today, the right direction).
- The `flux-tools/*` image family (erase, deblur, outpainting, vto) — a table
  extension once the kind exists.
- Finetune routes, webhooks (`webhook_url`/`webhook_secret`) — kaibo polls;
  it does not bind sockets or receive callbacks (stdio-only invariant).

## Testing

Offline-first like every other kind: unit tests over request construction and
response parsing (the `AsyncResponse`/`ResultResponse`/status-enum shapes
above), error mapping for each terminal status, and the expiring-URL fetch
path. Live probes are `#[ignore]`d and keyed (`BFL_API_KEY` or the config
key) — no key exists yet at the time of writing; live validation happens when
one does.

## Status log

- 2026-08-31 — spec written from the live OpenAPI dump; implementation
  delegated. No key yet; offline build first.
- 2026-08-31 — `src/bfl.rs` landed: the five starter ops, request/response parsing,
  and every terminal-status error, all offline and failing-first. Shape decided:
  hybrid inline-poll-then-`Deferred` — `generate` polls in-call for
  `INLINE_POLL_BUDGET` (30s) and falls back to the existing `Deferred`/`job-N` lane
  only past that, so the common fast case returns bytes and the rare slow one still
  gets a handle; this is the first shape from the spec's two options, with `Deferred`
  as its documented bound. `AsyncResponse.cost` rides `MediaOutcome::Complete`'s
  `note` channel on the fast path only — no note channel exists on the deferred
  path today, recorded as a future widening. Wired into `credentials.rs`,
  `media.rs`, `discover.rs`, `server/mod.rs`, and the config docs the same way
  #168 wired `gemini-images`. Still no `BFL_API_KEY`; `tests/bfl_live.rs` is
  `#[ignore]`d and ready for one.
- 2026-08-31 — review finding fixed: the poll leg shared `create`'s general
  client, so a plaintext or redirecting `polling_url` (provider-chosen,
  carrying the `x-key` credential) would have dialled or followed it. `poll_once`
  now runs over `crate::tls::artifact_fetch_client` (https-only, no redirects) —
  the same posture the `result.sample` fetch already had, extended to
  `polling_url`. Failing-first: a new transport test proved today's code makes a
  real connection to a plaintext `polling_url`; it passes now that the poll
  client refuses one before dialling.
