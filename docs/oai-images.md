# view_image on OpenAI-compatible VLMs (the user-turn image channel)

**Status:** IMPLEMENTED, offline-green (2026-06-12). The break-rewrite-resume path
ships in `src/consult.rs` (`ViewImageBreakHook`, `rewrite_view_image_history`, the
`run_phase` resume loop) gated on `ModelCaps.tool_result_images`; anthropic/gemini
keep the tool-result channel untouched. Offline tests cover the rewrite (separate
message, idempotency, co-tool-call) and the driven loop (break→resume, co-tool-call).
**The live probe below is the remaining gate** — mandatory before we call it done,
because the scripted mock can't catch an orphaned `tool_use`: offline-green ≠
live-works. Both spikes done (S2 made the image a *separate* user message). The design
notes below are kept as the as-built record.

## The promise we're keeping

kaibo is multi-provider: a `ProviderKind` is the wire protocol, and `openai` is
"any OpenAI-compatible endpoint" — hosted GPT, or a local llama.cpp/vLLM serving a
vision model. That promise includes **vision**: a Qwen-VL or other VLM behind an
`openai` backend should be able to *see* an image through `view_image`, the same as
Anthropic and Gemini do today. Right now it can't, and the example config's
`[casts.vlm]` (`vision = true` on a `llama` backend) implicitly promises a
capability that fails at call time. This doc closes that gap.

## Why it fails today — channel, not capability

`view_image` is a rig `Tool` (`src/view_image.rs`). Its output is the hybrid
envelope `{response, parts:[{type:"image", data, mimeType}]}` (`view_image.rs:158`).
rig parses any tool output through `from_tool_output`
(`rig-core/src/completion/message.rs:913`), which **only ever produces
`OneOrMany<ToolResultContent>`** — a tool's bytes can land in the conversation only
as *tool-result* content, never as user content.

That's fine for Anthropic and Gemini: images inside a tool result are a documented,
first-class feature there (Anthropic `tool_result` image blocks; Gemini
`functionResponse` inline data). It is **not** fine for OpenAI: the OpenAI wire
format forbids images in a `role:tool` message, and rig enforces this before
sending — `ToolResultContent::Image(_) => Err("OpenAI does not support images in
tool results. Tool results must be text.")`
(`rig-core/src/providers/openai/completion/mod.rs:460`). Even a permissive local
server never gets the chance; rig rejects it first.

The key correction to an earlier reading: this is **not** "OpenAI can't send images
to a model." OpenAI VLMs see images all the time — as `image_url` parts in **user**
messages. rig fully supports that path: a base64 `UserContent::Image` becomes a
`data:<mime>;base64,<…>` `image_url` (`openai/completion/mod.rs:483-516`). The image
is simply riding the wrong channel. The fix is to put it on the **user-turn**
channel, which every provider accepts.

## The enabling seams (why this is bounded, not a rewrite)

Two facts, both verified against rig-core 0.38.2, mean kaibo does **not** have to
reimplement rig's multi-turn agent loop:

1. **A terminating hook hands back the full transcript.** rig 0.38 added
   `PromptHook` (`rig-core/src/agent/prompt_request/hooks.rs`). On
   `HookAction::Terminate`, the loop returns
   `PromptError::prompt_cancelled(build_full_history(...), reason)` — i.e. the entire
   `Vec<Message>` so far (`prompt_request/mod.rs:669`). `HookAction` is only
   `Continue | Terminate` (`hooks.rs:207`); it can't rewrite history itself, but
   Terminate surfacing the transcript is all we need.
2. **`.with_history()` lets kaibo re-enter the managed loop.** kaibo already uses
   exactly this on the turn-cap path: `finalize_after_max_turns`
   (`src/consult.rs:782`) re-prompts with `.with_history(history).max_turns(1)`
   (`consult.rs:811`). The `MaxTurnsError` arm already pulls `chat_history`
   (`consult.rs:757`).

So rig keeps running the loop (concurrency, finalize, thinking-block handling); we
intervene only at a `view_image` call, rewrite one spot in the transcript, and hand
the loop back.

## Design: break → rewrite → resume

1. **Break — at the turn boundary, not mid-turn.** Install a `PromptHook` on the
   phase agent (today `run_phase` sets none). It does **not** terminate on
   `on_tool_result` the instant `view_image` runs — a single assistant turn can call
   `view_image` *and* `run_kaish` together, and breaking mid-turn would drop the
   other tool's execution, leaving an orphaned `tool_use` with no `tool_result` (a
   protocol violation on resume). Instead the hook flags "a `view_image` ran this
   turn" on `on_tool_result`, and terminates on the **next** `on_completion_call`
   (`HookAction::Terminate`). That boundary is where rig has already written *all* of
   the triggering turn's tool results into the transcript and is about to call the
   model again — and it's the path verified to return the full history
   (`prompt_request/mod.rs:669`). Gate the whole behavior on
   `transport_supports_tool_result_images(kind)` being false (see capability model).
2. **Rewrite.** kaibo receives the cancellation error carrying `chat_history`. It
   walks the transcript for each assistant `ToolCall` named `view_image` (the
   `tool_use_id` lives on the *assistant* `ToolCall`), finds the matching
   `UserContent::ToolResult { id }`, and transforms that single spot:
   - the tool result becomes **text** — a short ack: `"Loaded <label> (<mime>,
     <KiB>); shown below."` (satisfies the tool_use→tool_result requirement every
     provider has);
   - a **separate, tool-result-free `Message::User { [Image] }`** (the base64 bytes
     `view_image` already produced) is inserted immediately after — *not* mixed into
     the tool-results message (see Message shape / spike S2: rig drops non-tool
     content from a user turn that also carries tool results).

   All other content — assistant text/thinking blocks, other tools' `tool_use` and
   their `tool_result`s — is preserved verbatim, so no `tool_use` is left unanswered.
3. **Resume.** Re-enter the managed loop the way `finalize_after_max_turns` already
   does: split the rewritten transcript with the `finalize_prompt` pattern
   (`consult.rs:697`) — the trailing message becomes the `prompt`, the rest goes to
   `.with_history(...)` — so the original `user_prompt` is **not** replayed a second
   time on top of the history that already contains it. Pass the *remaining* turn
   budget (see turn-accounting note), not a fresh `max_turns`. The model now sees the
   image in user content. Break recurs only if `view_image` is called again.

### Message shape per provider (what rewrite must emit)

**The image must be a *separate* `Message::User`, never mixed with the tool result
— this is the load-bearing result of spike S2.** rig's openai converter
(`TryFrom<OneOrMany<UserContent>> for Vec<Message>`, `openai/completion/mod.rs:610`)
partitions a user turn's content into tool-results vs. everything-else and, *if both
are present, emits only the tool results and silently drops the rest* (lines 618-646,
with a code comment admitting it). So a rewritten turn of `[ToolResult(text),
Image]` would lose the image with no error — the exact silent-fallback failure we
refuse. The rewrite therefore produces two messages:

```
Message::User { [ ToolResult(view_image → text ack), <other tools' ToolResults> ] }
Message::User { [ Image(base64) ] }          # separate, tool-result-free
```

rig serializes that to the valid OpenAI sequence — assistant tool_call → `role:tool`
result(s) → `role:user` image (`data:` url) — because each user message is now
single-kind and partitions cleanly. anthropic/gemini are **not** rewritten under the
openai-only plan: they keep the idiomatic tool-result image and never break.

## Capability model: see ∧ transport

The current `view_image` gate is "model can see" (`ModelCaps.vision`,
`is_vision_capable` at `src/consult.rs:141`). That's necessary but not sufficient:
the real predicate is **model can see ∧ the chosen channel can carry the image**.
Introduce a small `transport_supports_tool_result_images(kind: ProviderKind) -> bool`
(anthropic/gemini → true, openai → false; deepseek is moot — vision-blind) and
branch on *that*, not on `kind == Openai`. Only one kind flips the switch today, but
a capability predicate means the next no-tool-result-image provider is a table entry,
not a new `if`. Two ways to satisfy the predicate:
- **openai-only rewrite (chosen first):** keep the tool-result channel for backends
  that support it (anthropic, gemini); for `openai` backends, attach `view_image`
  when `vision=true` *and* route its output through the break-rewrite-resume path.
  Anthropic/Gemini code is untouched.
- **uniform (later, optional):** always deliver images via the user-turn channel and
  drop tool-result images everywhere. One code path, but it disturbs the two proven
  providers and needs them re-probed. Defer until openai-only is solid.

Until the rewrite ships, `view_image` should **not** silently attach to an `openai`
vision slot and then 500 at call time — that's the footgun the `[casts.vlm]` example
sets. Either the rewrite lands, or the gate excludes openai (honest absence).

## Work plan

1. `PromptHook` impl (flag-on-`on_tool_result`, terminate-on-next-`on_completion_call`)
   + wire a hook slot through `run_phase` (none today). ~small.
2. History-rewrite fn: for each assistant `ToolCall` named `view_image`, rewrite its
   matching `UserContent::ToolResult` to a text ack and insert a user `Image` after
   it; preserve every other block (assistant text/thinking, other tools' use/result
   pairs). Pure, unit-testable. ~medium.
3. Resume orchestration in `run_phase`: **add the missing match arm** — today
   `run_phase` (`consult.rs:755`) handles only `Ok` / `MaxTurnsError` / catch-all
   `Err`, so the cancellation variant would fall straight through to
   `"model loop failed"` and the resume would be dead code. Add the arm; loop the
   prompt over it; split prompt/history via the `finalize_prompt` pattern to avoid a
   doubled user prompt; decrement an **outer** turn budget by turns consumed per
   resume (derive from the transcript — rig's history carries no `turns_used`), so a
   model that loops `view_image` can't inflate its budget. Keep the existing
   `MaxTurnsError`/`finalize_after_max_turns` behavior intact (and confirm finalize
   tolerates a user-content image in the trailing message). ~medium — the intricate
   part.
4. Caps/gating: `transport_supports_tool_result_images(kind)` predicate; openai
   `vision=true` attaches view_image and selects the rewrite path; anthropic/gemini
   stay on the tool-result channel. ~small.
5. Doc/config updates: `view_image.rs` module invariant (a seeing model on a
   no-tool-result-image transport gets the image via a user turn), and the
   `[casts.vlm]` example stops being a false promise.

## Tests

- **Offline, failing-first (the core regression):** a two-phase responder (the
  pattern the existing vision test already uses at `tests/consult.rs:927`) — call 1
  emits the `view_image` tool call; call 2 is the **resumed** request, where the
  responder asserts the transcript carries a `UserContent::Image` and **no**
  `ToolResultContent::Image`. The assertion must be **responder-side**, walking
  `req.chat_history` directly (a `request_has_user_image` helper alongside the
  existing `request_has_tool_result_image`) — `RecordedRequest` captures only *text*
  (`test_support.rs` `user_text`/`transcript`), so it can't observe image content.
  Necessary but **not sufficient**: the mock returns the scripted response regardless
  of protocol validity, so a rewrite that leaves an orphaned `tool_use` passes
  offline and only the live probe catches it.
- Keep the `tests/rig_wire.rs` canary (anthropic tool-result shape).
- **Live probe (mandatory, can't be faked offline):** a real openai-compatible VLM
  (local Qwen-VL via llama.cpp/vLLM) sees an injected user image and reports a detail
  only visible by looking. This is the same lesson the rig 0.34 duplicate-`type` bug
  taught: offline-green ≠ live-works, because the scripted client never exercises
  real provider serialization. Gated on a local VLM being up.

## Spikes — done (2026-06-12, rig-core 0.38.2)

- **S1 — terminate carries history: CONFIRMED, with the precise mechanism.** The
  variant is `PromptError::PromptCancelled { chat_history: Vec<Message>, reason }`
  (`completion/request.rs:114` — note an *unboxed* `Vec`, unlike `MaxTurnsError`'s
  `Box<Vec<…>>` at `:107`; `run_phase`'s new arm destructures accordingly). The agent
  loop accumulates `new_messages` across turns (`prompt_request/mod.rs:627`); a turn's
  tool calls land as an assistant message (`:803`) and **all** its tool results as a
  *single* `Message::User` (`:1081`). When the next turn's `on_completion_call` fires
  (`:665`), `new_messages` already holds both, and `Terminate` returns
  `build_full_history(...)` (`:670`) — the complete transcript. So the design's
  flag-on-`on_tool_result` / terminate-on-next-`on_completion_call` yields a transcript
  with no unanswered `tool_use`. Co-tool-call orphaning is structurally impossible.
- **S2 — openai mixed-content split: CONFIRMED, and it changed the design.** rig's
  `TryFrom<OneOrMany<UserContent>> for Vec<Message>` (`openai/completion/mod.rs:610`)
  partitions tool-results vs. other content and, **if both are present, emits only
  the tool results and silently drops the rest** (`:618-646`). So the image must be a
  *separate*, tool-result-free `Message::User` — see Message shape above. With that
  separation rig produces the valid `[role:tool, role:user]` sequence; mixed in one
  message it would have lost the image with no error. (This is why we spike.)

## Risks

- **Co-tool-call orphaning** — `view_image` + another tool in one assistant turn.
  Mitigated by breaking at the turn boundary (`on_completion_call`), not mid-turn, so
  every `tool_use` in the turn already has its `tool_result` before we rewrite. This
  is the subtlest hazard; the offline test must include a turn that calls `view_image`
  alongside `run_kaish`.
- **Turn-budget inflation** — rig's history carries no `turns_used`, so remaining
  budget is derived from the transcript; a naive "fresh `max_turns` per resume" lets a
  looping `view_image` run unbounded. Covered by the outer-budget accounting in
  work item 3.
- **Doubled user prompt on resume** — avoided by the `finalize_prompt` split
  (`consult.rs:697`); a regression here silently duplicates the question.
- **Thinking-block parity** — the rewrite must leave assistant thinking blocks
  untouched. If rig strips them from `chat_history`, that's a pre-existing limitation
  of the `MaxTurnsError` path, not a new one — but confirm.
- **Silent image-drop in a mixed user turn** — *resolved by design* via S2: the
  image goes in its own tool-result-free `Message::User`. The offline test must
  assert the image survives as `UserContent::Image` precisely to guard this.
- **Can't be fully proven offline** — the live probe is load-bearing, not optional;
  the offline test proves the rewrite shape, not protocol validity.

## Review

Reviewed via kaibo's own `consult` on the `deepseek` cast (v4-flash → v4-pro),
2026-06-12. Findings folded in above: the turn-boundary break (co-tool-call
orphaning), the missing `run_phase` cancellation arm, the prompt/history split to
avoid a doubled prompt, transcript-derived turn accounting, the capability-predicate
gate, and the responder-side image assertion (the recorder sees only text). The
kaibo-side file:line citations were spot-checked correct against the code.

## References

- kaibo: `src/view_image.rs` (envelope `:158`, `Tool::Output = Value` `:189`),
  `src/consult.rs` (`run_phase` `:730`, managed loop `:755`, `MaxTurnsError`
  `:757`, `finalize_after_max_turns` `:782`, `.with_history()` `:811`),
  vision gate (`ModelCaps`/`is_vision_capable`/`phase_tools` in `consult.rs`),
  `tests/rig_wire.rs`.
- rig-core 0.38.2: `from_tool_output` (`completion/message.rs:913`), openai
  tool-result-image rejection (`providers/openai/completion/mod.rs:460`), openai
  user-image base64→data-url (`:483-516`), `PromptHook`/`HookAction`
  (`agent/prompt_request/hooks.rs:79,207`), Terminate→`prompt_cancelled`
  (`agent/prompt_request/mod.rs:669`), anthropic tool-result image struct variant
  (`providers/anthropic/completion.rs:870`).
