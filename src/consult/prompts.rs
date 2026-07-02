//! Preambles and prompt framers for the consult phases.

use crate::config::ModelRole;
use crate::kaish_syntax::kaish_syntax_core;
use crate::session::QaTurn;

/// Splice the operator's house rules (if any) onto a phase preamble. The base
/// preamble functions stay pure (and their tests byte-for-byte stable); this is
/// the one seam that folds in the assembled `[context]` block. Every phase that
/// drives a model uses it — the `consult` driver, the toolless `oneshot`, *and* the
/// nested `explore′` sweep — so the explorer orients on the same guidance the driver
/// does (it helps *search*, not just the answer).
/// `None` returns the base unchanged: a server with no `[context]` files runs
/// exactly the historical preamble.
///
/// Framed as standing background, not the question, and positively (per the
/// `positive-prompt-framing` discipline): tell the model what the block *is* and
/// how to use it — conventions to honor while investigating — rather than fencing
/// it off. It sits *after* the base so the tool's own role framing leads.
fn with_house_rules(base: String, house_rules: Option<&str>) -> String {
    match house_rules {
        None => base,
        Some(rules) => format!(
            "{base}\n\n\
             --- Operator house rules for this codebase ---\n\
             The agent you're helping configured the guidance below — project \
             conventions and working preferences for this repository. Treat it as \
             trusted standing context: honor it as you investigate and when you write \
             your answer. It's background about how this codebase works, not the \
             question you're answering.\n\n{rules}"
        ),
    }
}

/// Operator preamble (system-prompt) overrides per phase, from the `[prompts]`
/// config table. `None` for a phase means "use the built-in" — so an empty table
/// is byte-for-byte the historical preambles. **Full replace** by decision: an
/// override *is* the role framing, verbatim; the kaish operating contract is not
/// re-appended here because it independently rides the `run_kaish` tool
/// description (`run_kaish_tool_description`), so the model keeps the shell
/// contract even when an operator rewrites the prose. Empty/whitespace values are
/// refused at config load (`config.rs::merge_prompts`) — a blank system prompt is
/// never the intent. House rules still append on top (see [`phase_preamble`]):
/// `[prompts]` replaces the *role* framing, `[context]` adds *project* guidance —
/// orthogonal axes.
#[derive(Debug, Clone, Default)]
pub struct PromptOverrides {
    /// Replaces [`report_preamble`] — the nested `explore′` sweep inside `consult`.
    pub explorer: Option<String>,
    /// Replaces [`consult_preamble`] — the `consult` driver.
    pub consult: Option<String>,
    /// Replaces [`oneshot_preamble`] — the thin, toolless `oneshot`.
    pub oneshot: Option<String>,
    /// Replaces [`batch_preamble`] — the offline, max-thinking `batch_submit`. A key
    /// of its own (not shared with `oneshot`) because the batch lane is a different
    /// behavioral contract: one response, no follow-up, spend on depth.
    pub batch: Option<String>,
}

/// Resolve one phase's full system prompt: the operator override if set, else the
/// built-in `default`, then the static repo `orientation` map, then house rules.
/// The single composition point for every model-driven phase, so override +
/// `[orientation]` + `[context]` layering is identical everywhere. Order: role
/// framing → the file map (immediately useful context) → operator house rules.
fn phase_preamble(
    override_: Option<&str>,
    default: fn() -> String,
    orientation: Option<&str>,
    house_rules: Option<&str>,
) -> String {
    let mut base = override_.map(str::to_string).unwrap_or_else(default);
    if let Some(map) = orientation {
        base.push_str("\n\n");
        base.push_str(map); // carries its own `PROJECT FILES.` header
    }
    with_house_rules(base, house_rules)
}

/// The model-driven phases whose system prompt kaibo composes. One enum so the three
/// per-phase decisions — *which* built-in default, *which* `[prompts]` override key,
/// and *whether* the phase reads the project (so the `[orientation]` map + `[context]`
/// house rules splice) — live in exactly one place, [`resolve_phase_preamble`]. Every
/// live tool routes through it, and so does the `kaibo://prompts` resource, so what the
/// resource shows can never drift from what a call actually sends the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// The explorer sweep: `explore`, the nested `explore′` inside `consult`, and
    /// `deliberate`'s dossier phase all share this one.
    Explorer,
    /// The `consult` driver.
    Consult,
    /// The thin, toolless `oneshot`.
    Oneshot,
    /// The offline synth: `batch_submit` and `deliberate`'s synth, on either lane.
    Batch,
}

impl Phase {
    /// The four phases, for callers that enumerate every prompt (the resource).
    pub const ALL: [Phase; 4] = [
        Phase::Explorer,
        Phase::Consult,
        Phase::Oneshot,
        Phase::Batch,
    ];

    /// A short, stable label for a phase — the resource header and the tools it drives.
    pub fn label(self) -> &'static str {
        match self {
            Phase::Explorer => "explorer (explore · consult sweep · deliberate dossier)",
            Phase::Consult => "consult driver",
            Phase::Oneshot => "oneshot",
            Phase::Batch => "batch / deliberate synth (offline)",
        }
    }

    /// The built-in default preamble for this phase.
    fn default_preamble(self) -> fn() -> String {
        match self {
            Phase::Explorer => report_preamble,
            Phase::Consult => consult_preamble,
            Phase::Oneshot => oneshot_preamble,
            Phase::Batch => batch_preamble,
        }
    }

    /// This phase's `[prompts]` override key (per-slot preamble already folded in
    /// upstream). Public so the `kaibo://prompts` resource can report which phases carry
    /// an active override without re-encoding the phase→key mapping.
    pub fn override_in(self, p: &PromptOverrides) -> Option<&str> {
        match self {
            Phase::Explorer => p.explorer.as_deref(),
            Phase::Consult => p.consult.as_deref(),
            Phase::Oneshot => p.oneshot.as_deref(),
            Phase::Batch => p.batch.as_deref(),
        }
    }

    /// Does this phase read the project? The explorer sweep and the `consult` driver
    /// do — so they get the `[orientation]` map and `[context]` house rules spliced.
    /// `oneshot` and the offline `batch` synth own their context (the caller supplies
    /// it), so neither project layer reaches them — the seam that used to sit as a bare
    /// `None, None` at each of those call sites now lives here, in one place.
    pub fn reads_project(self) -> bool {
        matches!(self, Phase::Explorer | Phase::Consult)
    }

    /// Which cast slot's `preamble` frames this phase: the **explorer** slot drives the
    /// explorer sweep; the **synth** slot drives every synth phase (`consult`, `oneshot`,
    /// and the offline `batch`/`deliberate` synth). Lets the `kaibo://prompts/{cast}`
    /// resource attribute a phase's framing to the slot that set it — the same slot→phase
    /// mapping [`crate::config::Cast::resolved_prompts`] applies.
    pub fn slot_role(self) -> ModelRole {
        match self {
            Phase::Explorer => ModelRole::Explorer,
            Phase::Consult | Phase::Oneshot | Phase::Batch => ModelRole::Synth,
        }
    }
}

/// Compose one phase's full system prompt through the single layering point. Picks the
/// operator override (else the built-in) for `phase`, then — for the project-reading
/// phases only — splices the `[orientation]` map and `[context]` house rules. This is
/// what every live tool builds and what the `kaibo://prompts` resource renders, so the
/// resource is exactly the code path, not a restatement of it.
pub fn resolve_phase_preamble(
    phase: Phase,
    prompts: &PromptOverrides,
    orientation: Option<&str>,
    house_rules: Option<&str>,
) -> String {
    // The phase decides whether the project layers apply — pass them unconditionally
    // and let `reads_project` gate, so no call site re-encodes that rule.
    let (orientation, house_rules) = if phase.reads_project() {
        (orientation, house_rules)
    } else {
        (None, None)
    };
    phase_preamble(
        phase.override_in(prompts),
        phase.default_preamble(),
        orientation,
        house_rules,
    )
}

/// Explorer preamble: gather and organize evidence, don't conclude. Composes the
/// shared [`kaish_syntax_core`] so the shell idioms and exit-code contract are
/// stated in exactly one place.
pub fn report_preamble() -> String {
    let core = kaish_syntax_core();
    format!(
        "You are a code explorer. You build a complete, accurate picture of the code \
         a question touches and hand it to a synthesizer who writes the final \
         answer — so your work is to gather grounded evidence and cite it exactly. \
         {core}\n\n\
         HOW TO READ. Read for the whole picture in as few looks as possible — the \
         context window is yours to fill, so read in wide passes. A short file: read \
         it WHOLE with `cat -n FILE` — one read hands you its imports, its context, \
         and exact line numbers together. A big file (`wc -l FILE` if unsure): walk it \
         in wide spans of a few hundred lines — `cat -n FILE | sed -n '1,400p'`, then \
         `'401,800p'`, then `'801,1200p'` — so each look lands a whole run of related \
         code together: a type with its impl, a function with the code around its call \
         sites, an import block with what uses it. If a whole-file read comes back \
         truncated (exit 3, a head+tail sample), it was too big for one look — walk it \
         in those wide spans. To locate something across files, take the surrounding \
         context in the same call — `grep -rn -B4 -A8 PATTERN .` returns each match \
         with the lines around it, ready to understand.\n\n\
         HOW TO INVESTIGATE. Aim for the complete set of relevant locations. Follow \
         each key symbol to where it is defined and where it is used; chase anything \
         that puzzles you until it is clear — a confusing spot usually hides the \
         thing you need. Follow each thread while you are already in the code, so one \
         thorough pass leaves you the complete picture.\n\n\
         WHAT TO PRODUCE. A curated report for the synthesizer, in these sections:\n\
         - SummaryOfFindings: what you concluded, in a few sentences.\n\
         - RelevantLocations: for each location that matters — the concrete \
         `file:line`, the key symbols there (functions, types, fields), a short \
         verbatim snippet, and what it means for the question.\n\
         - ExplorationTrace: the path you took, when it helps the synthesizer trust \
         the result.\n\
         Keep it tight and evidence-first. The synthesizer trusts your citations and \
         builds on them, so ground every claim in an exact `file:line` — that \
         exactness is the whole value of your report."
    )
}

/// Per-call loop tunables for a phase. Model-tracking knobs (`max_tokens`, the
/// thinking budget, sampling) ride each [`Arm`] (they track the slot's model);
/// what remains here are the loop bounds the caller may dial per request, the
/// sandbox limits, and the progress sink.
/// One caller-attached file, classified so the driver's prompt can route it to the
/// right tool. `consult` never inlines an attachment's bytes — text files the model
/// reads itself with `cat -n`, images it views with `view_image` (the image-analog of
/// `cat`, present whenever the synth is vision-capable). The server sniffs each file's
/// magic bytes to set `is_image`, so a `.png` named `.txt` (or vice versa) is routed by
/// content, not extension — matching how `view_image` re-sniffs authoritatively at read.
#[derive(Debug, Clone)]
pub struct ConsultAttachment {
    /// The path the model passes to `cat -n` or `view_image`: root-relative for a file
    /// under the project root, the one real tree the consult shell mounts.
    pub path: String,
    /// True when the file sniffed as a known image type — route it to `view_image`, not
    /// the shell. A consult that carries an image attachment must run a vision-capable
    /// synth (the server gates this up front, the same honest refusal `oneshot`/`batch`
    /// give a blind model).
    pub is_image: bool,
}

/// The `oneshot` preamble: a thin, direct second opinion with no tools and no
/// codebase access. The caller owns the context, so this never investigates — frame
/// the model as a capable outside voice answering from what it was handed plus its
/// own knowledge. Deliberately minimal: no kaish cheatsheet (there are no tools to
/// drive) and no repo map (oneshot never reads the project).
pub fn oneshot_preamble() -> String {
    "You are a capable model giving a direct second opinion to another agent. Answer \
     the question it sends from the material it provides and your own knowledge — \
     this call has no codebase access and no tools, so the caller owns the context. \
     Be precise and useful: reason over exactly what you were handed, and name \
     explicitly anything you'd need that wasn't given, so the caller can supply it \
     next turn. Keep your claims grounded in the material and say where its edge is."
        .to_string()
}

/// The `batch` preamble: a capable model answering one hard question *offline*, at
/// max thinking, with no codebase access and no tools. Deliberately **not** a reuse of
/// [`oneshot_preamble`] — batch is the same toolless shape but a different behavioral
/// contract, and a cross-model review of the feature caught three places the oneshot
/// wording misfires for the async lane:
///
/// - **No follow-up turn.** A batch item is answered once, offline; the caller cannot
///   clarify and there is no next turn. oneshot's "name what you'd need rather than
///   guessing" is right *synchronously* (flagging a gap invites the caller to fill it
///   next turn) but wrong here — stopping at "I'd need X" burns the caller's one shot
///   for nothing. The batch contract is *state the assumption, answer under it, flag
///   what would change* — both the answer and the diagnostic, in one pass.
/// - **Depth is free.** The lane forces high effort + a generous token floor precisely
///   because the latency is already accepted. The prompt says so out loud — spend the
///   room on depth — rather than leaving that intent only in the knobs.
/// - **Primary answer, not a footnote.** Batch is for asking the best model the hard
///   question, so the "second opinion" framing under-positions it; the load-bearing
///   part is "for another agent" (an external advisor owns no context), which we keep.
///
/// Positive framing throughout (the CLAUDE.md rule): the old "rather than guessing it"
/// named the unwanted pathway; the replacement asks for the wanted behavior — a
/// reasoned, labelled assumption — directly.
pub fn batch_preamble() -> String {
    "You are a capable model answering a hard question for another agent, offline. Work \
     from the material it provides and your own knowledge — this call has no codebase \
     access and no tools, so the caller owns all the context you have. This is your \
     single response: there is no follow-up turn and the caller cannot clarify, so make \
     the answer complete and self-contained, and spend the room you have on depth — \
     reason the problem all the way through. Be direct and precise. Ground every claim \
     in the material or your own knowledge, and say where the evidence runs out. Where \
     something you'd need is missing, state the assumption you're making, answer under \
     it, and flag what would change if the assumption is wrong."
        .to_string()
}

/// Resolve the `batch` phase's system prompt: the operator `[prompts].batch` override
/// if set, else the built-in [`batch_preamble`]. Batch reads no project (the `oneshot`
/// shape), so neither the repo map nor house rules splice — the same composition
/// `oneshot` gets, exposed as a public seam because the batch path lives outside the
/// `ConsultConfig`-driven loop (it runs on the provider's batch lane, not [`Arm::run`]).
pub fn batch_system_prompt(override_: Option<&str>) -> String {
    // Route through the shared `Phase` seam so the `Batch` framing (built-in vs
    // override, project layers off) is decided in exactly one place — the same one the
    // resource renders. This path carries a bare override rather than a full
    // `PromptOverrides`, so wrap it in the one key `Phase::Batch` reads.
    let prompts = PromptOverrides {
        batch: override_.map(str::to_string),
        ..Default::default()
    };
    resolve_phase_preamble(Phase::Batch, &prompts, None, None)
}

/// Build the consult driver's user prompt from the question, any caller-supplied
/// `context`, and any prior session turns. Pure and offline-testable: this framing
/// is the whole of the context-seed and multi-turn hand-off, so it's worth pinning.
///
/// With **no** context and **no** history this is exactly the bare question — a
/// stateless, unseeded consult is byte-for-byte unchanged. Supplied `context`
/// (a diff summary, a prior report, pasted source) is framed as *trusted starting
/// evidence*: a grounded `file:line` rarely needs re-deriving, and the steer is to
/// investigate for *more* when the context isn't enough — the CLAUDE.md acquisition,
/// not verification, posture. History prepends the prior `(question, answer)` pairs
/// and steers the model to re-confirm any span a prior answer cited: the exploration
/// runs fresh every turn (we never replay the stored report — it'd be stale), so the
/// code is the ground truth, not the old answer.
pub fn consult_user_prompt(
    question: &str,
    context: Option<&str>,
    history: &[QaTurn],
    attached: &[ConsultAttachment],
) -> String {
    let context = context.map(str::trim).filter(|c| !c.is_empty());
    if history.is_empty() && context.is_none() && attached.is_empty() {
        return question.to_string();
    }
    let mut prompt = String::new();
    if !history.is_empty() {
        prompt.push_str(
            "This is a continuing conversation about the same codebase. Earlier turns, \
             oldest first:\n\n",
        );
        for (i, turn) in history.iter().enumerate() {
            prompt.push_str(&format!(
                "[Turn {}]\nQ: {}\nA: {}\n\n",
                i + 1,
                turn.question,
                turn.answer
            ));
        }
        prompt.push_str(
            "Use the earlier turns for context and continuity. Investigate fresh and \
             re-confirm any `file:line` an earlier answer cited before you rely on it — \
             the code is the ground truth, not the prior answer.\n\n",
        );
    }
    if let Some(context) = context {
        prompt.push_str(&format!(
            "Context the caller supplied (a diff or change summary, a prior report, or \
             pasted source):\n{context}\n\n\
             Treat it as trusted starting evidence: when it cites a concrete \
             `file:line`, trust it rather than re-deriving it. Reach for your tools \
             when you need more than it gives — a span it references but doesn't quote, \
             a whole file for the full picture, a detail it left open, or anything the \
             question reaches that it didn't cover. Where the code you read and the \
             context genuinely disagree, the code wins.\n\n",
        ));
    }
    if !attached.is_empty() {
        let (images, texts): (Vec<&ConsultAttachment>, Vec<&ConsultAttachment>) =
            attached.iter().partition(|a| a.is_image);
        prompt.push_str(
            "The caller attached these files as central to the question. They live under \
             the project root, so a relative path opens directly. Open each as you build \
             your answer:\n",
        );
        if !texts.is_empty() {
            prompt.push_str("\nText files — read each in full with the shell (`cat -n PATH`):\n");
            for a in &texts {
                prompt.push_str(&format!("- {}\n", a.path));
            }
        }
        if !images.is_empty() {
            // Images are binary — `cat` refuses them; the model has a `view_image` tool
            // (present because the synth is vision-capable, gated server-side) that hands
            // it the actual picture. Route images there, never to the shell.
            prompt.push_str(
                "\nImages — view each with the `view_image` tool (`view_image PATH`), which \
                 hands you the picture itself; don't `cat` an image:\n",
            );
            for a in &images {
                prompt.push_str(&format!("- {}\n", a.path));
            }
        }
        prompt.push('\n');
    }
    prompt.push_str(&format!("Now answer the current question:\n\n{question}"));
    prompt
}

/// The recomposed `consult` driver: one capable model, two tools. Composes the
/// shared [`kaish_syntax_core`] (for `run_kaish`) and frames `explore` as the way
/// to cover breadth. Positive framing on purpose — weaker/local models loop on
/// blanket prohibitions, so reinforce the grounded behavior we want.
pub fn consult_preamble() -> String {
    let core = kaish_syntax_core();
    format!(
        "You answer a question about a codebase, grounded in evidence and citing \
         concrete `file:line`. {core}\n\n\
         You also have a second tool, `explore`: it delegates a broad sweep to a \
         fast investigator that rips through the repo and reports back with a \
         curated report — RelevantLocations carrying `file:line`, key symbols, and \
         snippets. Reach for `explore` to cover breadth — find where a \
         thing lives, gather the relevant files — and use `run_kaish` to read the \
         code yourself. When you read directly, read generously in wide passes — a \
         short file whole with `cat -n FILE`, a big one in spans of a few hundred \
         lines (`cat -n FILE | sed -n '1,400p'`, then `'401,800p'`) — so each look \
         lands the code in its context. Build your answer from what \
         they return: quote the key snippet, name its `file:line`, and let the \
         evidence carry the claim. Where the evidence settles the question, answer \
         it fully; where it reaches its edge, say so and name what would close the gap.\n\n\
         The caller may hand you CONTEXT — a diff or change summary, a prior report, \
         or pasted source. Treat it as trusted starting evidence: when it cites a \
         concrete `file:line`, trust it rather than re-deriving it, and spend your \
         turns getting *more* than it gave — reading a span it left unquoted, a whole \
         file for the full picture, anything the question reaches past it. Where the \
         code you read and the context genuinely disagree, the code wins."
    )
}

/// Frame a built dossier + the original question into the offline synth's single
/// user turn — the whole of what `deliberate`'s heavyweight synth reasons over, on
/// either lane (a batch item's prompt, or the direct lane's one local completion).
/// Pure, so the wire shape is pinned without a network.
///
/// The framing installs the deliberate posture: the dossier is *trusted* investigated
/// evidence (a fast explorer read the real spans and cited them), so the synth spends
/// its one offline turn reasoning the question all the way through, not re-verifying
/// cites it can't cheaply re-derive — and names the edge of the evidence rather than
/// guessing past it (the "thin dossier deliberating on air" failure the spec warns of).
pub fn deliberation_prompt(question: &str, dossier: &str) -> String {
    format!(
        "A fast explorer investigated this codebase READ-ONLY and assembled the dossier \
         below — spans it read from the real, current source, cited by `file:line`. Trust \
         those citations as accurate and deliberate deeply over the question using this \
         evidence: reason it all the way through, and say where the evidence runs out. If \
         the dossier leaves a load-bearing detail open, reason under a stated assumption \
         and flag what would change if it's wrong.\n\n\
         ## Question\n{question}\n\n## Dossier\n{dossier}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The deliberation prompt is the whole context the offline synth reasons over, so
    /// pin its shape: both the question and the dossier survive, the question is read
    /// before the evidence, and the trust-the-cites posture is installed (the guard
    /// against a synth burning its one turn re-verifying, or deliberating on air).
    #[test]
    fn deliberation_prompt_carries_question_then_dossier_and_frames_trust() {
        let p = deliberation_prompt("Is the retry path safe?", "src/retry.rs:12 fn retry()");
        assert!(
            p.contains("Is the retry path safe?"),
            "question present: {p}"
        );
        assert!(
            p.contains("src/retry.rs:12 fn retry()"),
            "dossier present: {p}"
        );
        let q = p.find("## Question").expect("has a Question section");
        let d = p.find("## Dossier").expect("has a Dossier section");
        assert!(
            q < d,
            "the question is framed before the dossier evidence: {p}"
        );
        assert!(
            p.contains("Trust") && p.contains("evidence"),
            "installs the trusted-evidence posture: {p}"
        );
    }

    /// The explorer preamble carries the behaviors we measured into it — the
    /// whole-file reading directive (the lite-explorer win, 48→23 turns), the
    /// context-buffer `grep` idiom, and the three report sections the synth side now
    /// expects. Pure and offline; pins the prose so a future edit can't silently
    /// drop any of it (the synth preambles are written against this shape).
    #[test]
    fn report_preamble_keeps_the_reading_directive_and_report_shape() {
        let p = report_preamble();
        // Reading strategy: whole (short) files, wide spans for big ones, grep buffer.
        assert!(p.contains("cat -n FILE"), "whole-file read idiom: {p}");
        assert!(
            p.to_lowercase().contains("whole"),
            "the whole-file directive must survive: {p}"
        );
        assert!(
            p.contains("sed -n '1,400p'"),
            "big-file wide-span idiom: {p}"
        );
        assert!(
            p.contains("grep -rn -B4 -A8"),
            "grep context-buffer idiom: {p}"
        );
        // The report template the consult driver preamble is written
        // against — keep the three section names in lockstep with those.
        for section in ["SummaryOfFindings", "RelevantLocations", "ExplorationTrace"] {
            assert!(p.contains(section), "missing report section {section}: {p}");
        }
    }

    /// The batch preamble encodes the async lane's distinct contract — the three things
    /// a cross-model review flagged the oneshot wording getting wrong for batch. These
    /// are behavioral promises, so they get a test that fails if the prose drifts back
    /// toward the synchronous oneshot framing.
    #[test]
    fn batch_preamble_encodes_the_offline_one_shot_contract() {
        let p = batch_preamble();
        let lower = p.to_lowercase();
        // (1) No follow-up turn — be complete and self-contained in one response.
        assert!(
            lower.contains("single response") && lower.contains("no follow-up"),
            "batch must tell the model it gets exactly one offline response: {p}"
        );
        // (2) Depth is free here — spend the budget the lane forces on.
        assert!(lower.contains("depth"), "batch must ask for depth: {p}");
        // (3) Assume-and-answer, not flag-and-stall: state the assumption and answer
        // under it (the synchronous oneshot would say "name what you'd need").
        assert!(
            lower.contains("assumption") && lower.contains("answer under it"),
            "batch must steer toward assume-and-answer, not flag-and-stall: {p}"
        );
        // Positive framing (the CLAUDE.md rule): it must not reintroduce the negative
        // "rather than guessing" pathway the oneshot line used.
        assert!(
            !lower.contains("guess"),
            "batch preamble must stay positively framed — no 'guess' pathway: {p}"
        );
        // Still the toolless, contextless shape it shares with oneshot.
        assert!(
            lower.contains("no codebase access") && lower.contains("no tools"),
            "batch is the toolless, contextless shape: {p}"
        );
    }

    /// `[prompts].batch` fully replaces the built-in batch preamble; absent, the
    /// built-in stands. Batch reads no project, so nothing else splices.
    #[test]
    fn batch_system_prompt_honors_the_override() {
        assert_eq!(batch_system_prompt(None), batch_preamble());
        assert_eq!(
            batch_system_prompt(Some("custom batch frame")),
            "custom batch frame"
        );
    }

    /// The single `Phase` seam both the tools and the `kaibo://prompts` resource go
    /// through: each phase resolves to its own built-in default, an override wins per
    /// key, and the `[orientation]`/`[context]` project layers splice *only* for the
    /// project-reading phases (explorer + consult) — never for the caller-owns-context
    /// phases (oneshot + the offline batch synth), even when the layers are passed.
    #[test]
    fn resolve_phase_preamble_routes_each_phase_and_gates_project_layers() {
        let base = PromptOverrides::default();
        assert_eq!(
            resolve_phase_preamble(Phase::Explorer, &base, None, None),
            report_preamble()
        );
        assert_eq!(
            resolve_phase_preamble(Phase::Consult, &base, None, None),
            consult_preamble()
        );
        assert_eq!(
            resolve_phase_preamble(Phase::Oneshot, &base, None, None),
            oneshot_preamble()
        );
        assert_eq!(
            resolve_phase_preamble(Phase::Batch, &base, None, None),
            batch_preamble()
        );

        let map = "PROJECT FILES.\nsrc/lib.rs";
        let rules = "operator house rule";
        // The reading phases splice both project layers.
        for phase in [Phase::Explorer, Phase::Consult] {
            let p = resolve_phase_preamble(phase, &base, Some(map), Some(rules));
            assert!(
                p.contains(map) && p.contains(rules),
                "{} must splice the project layers",
                phase.label()
            );
            assert!(phase.reads_project());
        }
        // The context-owning phases drop them even when passed.
        for phase in [Phase::Oneshot, Phase::Batch] {
            assert_eq!(
                resolve_phase_preamble(phase, &base, Some(map), Some(rules)),
                resolve_phase_preamble(phase, &base, None, None),
                "{} must ignore the project layers",
                phase.label()
            );
            assert!(!phase.reads_project());
        }

        // An override wins over the built-in, per key.
        let ov = PromptOverrides {
            consult: Some("CUSTOM DRIVER".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_phase_preamble(Phase::Consult, &ov, None, None),
            "CUSTOM DRIVER"
        );
        // ...and doesn't bleed into a sibling phase.
        assert_eq!(
            resolve_phase_preamble(Phase::Oneshot, &ov, None, None),
            oneshot_preamble()
        );
    }

    /// No session history ⇒ the prompt is *exactly* the bare question. This pins the
    /// promise that a stateless consult is byte-for-byte its pre-session behavior.
    #[test]
    fn empty_history_yields_the_bare_question() {
        assert_eq!(
            consult_user_prompt("Where is the sandbox enforced?", None, &[], &[]),
            "Where is the sandbox enforced?"
        );
    }

    /// Text attachments are named in the prompt (for the model to `cat` itself) and
    /// steered to read them in full — and they're listed before the question, like context.
    #[test]
    fn attached_files_are_named_for_the_model_to_read() {
        let prompt = consult_user_prompt(
            "Does the diff weaken the sandbox?",
            None,
            &[],
            &[text_attach("changes.diff"), text_attach("src/sandbox.rs")],
        );
        assert!(
            prompt.contains("changes.diff"),
            "names each attached file:\n{prompt}"
        );
        assert!(
            prompt.contains("src/sandbox.rs"),
            "names each attached file:\n{prompt}"
        );
        assert!(
            prompt.contains("cat -n"),
            "steers the model to read them with the shell:\n{prompt}"
        );
        let listed = prompt.find("changes.diff").unwrap();
        let question = prompt.find("Does the diff weaken").unwrap();
        assert!(
            listed < question,
            "attachments precede the question:\n{prompt}"
        );
    }

    /// An image attachment must be routed to `view_image`, never to `cat` (which refuses
    /// binary). With a mix, each file lands under the right instruction: text → `cat -n`,
    /// image → `view_image`. This is the prompt half of the image-attach support; the
    /// server gates a vision-blind synth before we ever get here.
    #[test]
    fn image_attachments_are_routed_to_view_image_not_cat() {
        let prompt = consult_user_prompt(
            "What does the banner show, and does it match the brand doc?",
            None,
            &[],
            &[
                image_attach("docs/brand/banner-teal.png"),
                text_attach("docs/brand/README.md"),
            ],
        );
        // The image is steered to view_image and explicitly kept away from the shell.
        let view_at = prompt
            .find("view_image")
            .expect("image must be routed to view_image");
        let img_at = prompt
            .find("banner-teal.png")
            .expect("image is named in the prompt");
        // The image name sits under the view_image instruction, not the cat -n one.
        let cat_at = prompt.find("cat -n").expect("text section names cat -n");
        assert!(
            view_at < img_at,
            "the image is listed under the view_image instruction:\n{prompt}"
        );
        // The text file is under the cat -n section.
        let readme_at = prompt
            .find("README.md")
            .expect("text file is named in the prompt");
        assert!(
            cat_at < readme_at,
            "the text file is listed under the cat -n instruction:\n{prompt}"
        );
    }

    fn text_attach(path: &str) -> ConsultAttachment {
        ConsultAttachment {
            path: path.to_string(),
            is_image: false,
        }
    }

    fn image_attach(path: &str) -> ConsultAttachment {
        ConsultAttachment {
            path: path.to_string(),
            is_image: true,
        }
    }

    /// With history, every prior turn appears, the current question appears, and the
    /// turns precede the current question (the model reads context before the ask).
    #[test]
    fn history_is_replayed_before_the_current_question_in_order() {
        let history = vec![
            QaTurn::new("What is kaish?", "A read-only shell (src/sandbox.rs)."),
            QaTurn::new("Who calls it?", "consult drives it (src/consult.rs)."),
        ];
        let prompt = consult_user_prompt("And explore?", None, &history, &[]);

        for needle in [
            "What is kaish?",
            "A read-only shell (src/sandbox.rs).",
            "Who calls it?",
            "consult drives it (src/consult.rs).",
            "And explore?",
        ] {
            assert!(
                prompt.contains(needle),
                "prompt must carry {needle:?}:\n{prompt}"
            );
        }
        // Ordering: the first prior turn comes before the second, and both come
        // before the current question.
        let first = prompt.find("What is kaish?").unwrap();
        let second = prompt.find("Who calls it?").unwrap();
        let current = prompt.find("And explore?").unwrap();
        assert!(first < second, "turns must be oldest-first");
        assert!(
            second < current,
            "history must precede the current question"
        );
    }
}
