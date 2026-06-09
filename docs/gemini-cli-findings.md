# gemini-cli findings — per-model prompt & param design

What gemini-cli does for system prompts and per-model configuration, and which
ideas kaibo should borrow. Source read read-only at
`/home/atobey/src/research/gemini-cli` on 2026-06-09 (the npm `@google/gemini-cli`
checkout). Citations are `file:line` into that tree unless noted as kaibo.

Motivation: kaibo wants to *fit* each model rather than send one shape to all four
providers. gemini-cli is a single-provider tool, but it varies prompt, thinking
config, and tool schemas across Gemini *generations* — exactly the intra-kind split
kaibo faces (Gemini 2.5 vs 3). The direct twin of kaibo's `explore` phase is
gemini-cli's **codebase-investigator** sub-agent, so its wording is the most
directly stealable thing here.

---

## 1. System prompt construction — composed, not monolithic, overridable

- Entry point `PromptProvider.getCoreSystemPrompt()` —
  `packages/core/src/prompts/promptProvider.ts:47`.
- **File override:** if `GEMINI_SYSTEM_MD` is set, the prompt is loaded from that
  file instead of composed (`promptProvider.ts:53-119`). `GEMINI_WRITE_SYSTEM_MD`
  dumps the *assembled* prompt back to a file — a debug affordance for inspecting
  what the model actually saw.
- **Composition:** otherwise built from ~10 orthogonal sections via an options
  object — `preamble`, `coreMandates`, `subAgents`, `agentSkills`,
  `primaryWorkflows`, `planningWorkflow`, `operationalGuidelines`, `sandbox`,
  `interactiveYoloMode`, `gitRepo` (`promptProvider.ts:142-256`). Each section is a
  pure render fn (`renderPreamble`, `renderCoreMandates`, …) assembled in
  `snippets.ts:136-163`. Mode changes (plan vs normal) just change which renderers
  fire.

**kaibo read:** the composition machinery (10 snippet sections) is heavier than
"one primitive, four tools" wants — kaibo's preambles are short and already
parameterized into `run_phase` (`consult.rs:260`). Don't import the snippet
framework. **Do** borrow the *override chain idea* (file/env → composed default)
for the eventual config-overridable-prompts work, and the `WRITE_SYSTEM_MD` dump
as a debugging affordance once prompts vary per model.

---

## 2. Per-model customization — capability-based selection (the backbone idea)

gemini-cli keys prompt template, thinking config, **and** tool schemas off a
model-family classifier rather than scattering model-id checks:

- `supportsModernFeatures(model)` — `packages/core/src/config/models.ts:493`;
  `isGemini3Model`, `isCustomModel` feed it. Constants: `PREVIEW_GEMINI_MODEL =
  'gemini-3-pro-preview'`, `DEFAULT_GEMINI_MODEL = 'gemini-2.5-pro'`,
  `DEFAULT_THINKING_MODE = 8192` (`models.ts:54-118`).
- **Prompt swap:** `const activeSnippets = isModernModel ? snippets :
  legacySnippets` (`promptProvider.ts:73-82`) — modern (Gemini 3) vs legacy (2.x)
  system prompt entirely.
- **Thinking per family** (`codebase-investigator.ts:100-108`):
  ```ts
  thinkingConfig: supportsModernFeatures(model)
    ? { includeThoughts: true, thinkingLevel: ThinkingLevel.HIGH }   // Gemini 3
    : { includeThoughts: true, thinkingBudget: DEFAULT_THINKING_MODE } // 2.x
  ```
  Gemini 3 uses `thinkingLevel`; 2.x uses `thinkingBudget` (mutually exclusive).

**kaibo read:** this is the one structural idea to steal. kaibo's
`thinking_params(kind, budget)` (`consult.rs:146`) keys on `ProviderKind` alone;
the 2.5-vs-3 split is an *intra-kind* version difference of the same shape.
Generalize: a `Dialect` resolved per (kind, model). **Caveat the boundary:** kaibo's
current default synth `gemini-3.5-flash` *accepted* `thinkingBudget` in the
2026-06-06 live test, so the conservative classifier is `gemini-3-*` →
`thinkingLevel`, `3.5-flash`/2.x → `thinkingBudget`. Don't switch a working default
on a guess.

---

## 3. Tool definitions — per-model schema overrides (defer for kaibo)

Tools carry a `base` declaration plus an optional `overrides(modelId)`; a resolver
deep-merges them at request time:

- `resolveToolDeclaration(definition, modelId)` —
  `packages/core/src/tools/definitions/resolver.ts:17-34`; tools call it from
  `getSchema(modelId)` (e.g. `edit.ts:1148`).
- The registry passes the active model id when collecting schemas
  (`tool-registry.ts:678,717`).
- Per-family tool text lives in `model-family-sets/gemini-3.ts` vs
  `default-legacy.ts`.

**kaibo read:** kaibo's tool descriptions are static (`run_kaish` in
`explorer.rs`, `explore` in `consult.rs:426`), and the kaish cheatsheet is one
source of truth (`kaish_syntax.rs::kaish_syntax_core`). Per-model tool shaping is
real machinery for a payoff kaibo hasn't shown it needs. Defer until a probe shows
a model genuinely prefers a different tool shape (Amy's own bar: "if it looks like
a model has a preferred shape").

---

## 4. Generation config — temperature/topP are set; kaibo sets neither

The investigator runs at `temperature: 0.1, topP: 0.95` plus the per-family
`thinkingConfig` (`codebase-investigator.ts:97-109`), with `runConfig` bounds
`maxTimeMinutes: 10, maxTurns: 50` and read-only tools only (`:112-125`). Config is
a `ModelConfig { model, generateContentConfig }` (`modelConfigService.ts:42`),
deep-merged: global model defaults ← per-agent override ← per-request.

**kaibo read:** kaibo sets only `max_tokens` + `thinking` (`run_phase`,
`consult.rs:275-283`) — **no temperature, no topP**. Low temp for deterministic
code reading is a textbook fit and a natural `Dialect` knob. Add it via the seam;
probe whether Gemini wants it more than Anthropic before defaulting it on.

---

## 5. Model abstraction & fallback

`ContentGenerator` interface (`core/contentGenerator.ts:37-59`) abstracts the
provider (OAuth / API key / Vertex / gateway), wrapped by logging/recording/fake
decorators. Fallback is retry + content-validation re-ask + model fallback
(`fallback/handler.ts`, `geminiChat.ts`); a classifier can route a task to Flash
vs Pro. No explicit cheap→capable chain in the prompt layer.

**kaibo read:** kaibo already has the equivalent — `with_provider_client!`
(`consult.rs:45`) is the provider abstraction, and the explorer(cheap)→synth(capable)
split is structural, not a runtime classifier. Nothing to borrow here.

---

## 6. The codebase-investigator prompt — the direct twin of `explore`

`packages/core/src/agents/codebase-investigator.ts:132-190`. Techniques, ranked by
expected lift for kaibo's explorer, with the tension against our discipline noted:

1. **Structured report + a worked few-shot example** (`:166-189`). Final report is a
   JSON schema — `RelevantLocations:[{FilePath, Reasoning, KeySymbols}]`,
   `SummaryOfFindings`, `ExplorationTrace` — with a *full filled-in example in the
   prompt*. kaibo's explorer returns free-text "a curated report"
   (`report_preamble`); weaker/local models follow a *shown* shape far better than a
   *described* one. Highest value, highest blast radius (it reshapes the
   explorer→synth hand-off seam).
2. **"Treat confusion as a signal to dig deeper"** (`:146`) — *"If you find
   something you don't understand, you MUST prioritize investigating it."* This is
   kaibo's "get more when the context isn't enough," made imperative. Positive
   framing, aligns with our acquisition-over-verification discipline.
3. **Completeness pressure** (`:140,147`) — "DO NOT stop at the first relevant
   file… complete and minimal set." For the cheap explorer (coverage is its job)
   this may be the right lean.
4. **Tension to test, not adopt:** gemini-cli uses `DO / DO NOT` bullets freely
   *with Gemini* (`:139-140`) and it works — against kaibo's positive-framing
   discipline (Gemma fixates on prohibitions). Hypothesis: that caution is
   **Gemma-specific, not Gemini-wide**. If Gemini tolerates prohibitions its prose
   can be more directive than the local-Gemma profile's. Measure, don't assume.
5. **`<scratchpad>` mandate** (`:151-160`) — Checklist + "Questions to Resolve,"
   "complete ONLY when the list is empty." Strong scaffold for a less self-directed
   model, but pulls toward *long chats* — against kaibo's "few high-value turns" and
   the turn cap. High variance; probe last.

---

## What kaibo should do (decided 2026-06-09)

- **Params first, then probe prose.** Anthropic stays the exemplar because it's
  flexible; don't fork preamble prose on a hunch.
- **Build a `Dialect`** — per (kind, model) request shaping resolved from `Profile`.
  `thinking_params` is its first method (model-aware, *per phase* — see the
  `consult.rs:815` shared-thinking wrinkle). temperature/topP next. Preamble
  variants and tool-shape tweaks only as a probe earns them.
- **Don't import** the snippet-composition framework or per-tool override resolver
  yet — heavier than "one primitive, four tools" needs.
- Tracked in `docs/issues.md` ("Per-model request shaping" + "Per-model prose
  fitting" entries). The config-overridable-prompts P4 entry rides the same seam.
