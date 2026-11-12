//! The Decisions client — `decide`'s wire, driven directly rather than through rig.
//!
//! A Decisions API takes application `state` plus a map of typed `questions` and returns
//! probabilities instead of text: `choice` (one label of up to 255), `score` (a level on
//! an ordered scale of 2 to 10), and `noul` (the probability a statement is true). No
//! model writes a sentence, so there is no preamble, no tool loop, and no thinking
//! budget; this is not a completion phase and does not go through `run_phase`.
//!
//! Two routes speak it, one [`DecisionsWire`] each:
//!
//! - OpenRouter's `POST /api/alpha/decisions` (model ids `typesafe/jev-1.13`,
//!   `~typesafe/jev-latest`), bearer key, the context under `state`. Shapes confirmed by a
//!   live probe on 2026-10-06.
//! - Google's Gemini Decisions API, `POST /v1beta/decisions` (model id
//!   `gemini-decision-1`), `x-goog-api-key`, the context under `input` as text, and no
//!   `model` or `cost` in the reply. Shapes confirmed by a live probe on 2026-11-12, the
//!   day after launch.
//!
//! The 2026-10-06 probe also found what OpenRouter does not check, so this module checks
//! it instead, for both routes:
//!
//! - a `choice` with one option is accepted and answers `1.0` for it, so [`Question`]
//!   refuses fewer than 2 options before any request leaves;
//! - unknown request fields are ignored, so a misspelled field is silent. The request
//!   type here is closed and fixed;
//! - validation failures return the provider's zod error list as a JSON string in
//!   `error.message`, which [`DecideError::Provider`] carries verbatim.
//!
//! Failures are loud: a response whose answers do not match the questions asked (a missing
//! answer, a wrong type, a `choice` outside the options) is an error naming the question,
//! never a defaulted value. A caller thresholding on probabilities must not see a
//! fabricated one.
//!
//! The client is independent of the MCP tool in `server/mod.rs` so a later caller (an
//! agent loop that wants a quick check between turns) can reuse it.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Context;
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};

/// OpenRouter's Decisions endpoint.
pub const OPENROUTER_DECISIONS_URL: &str = "https://openrouter.ai/api/alpha/decisions";

/// Google's Gemini Decisions endpoint, on the default Gemini host.
pub const GEMINI_DECISIONS_URL: &str =
    "https://generativelanguage.googleapis.com/v1beta/decisions";

/// Which Decisions API a request speaks: the field names, the key header, and the reply
/// shape differ between the two, while the question types are the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionsWire {
    /// OpenRouter: bearer key, context under `state`, `provider` routing preferences.
    OpenRouter,
    /// Gemini: `x-goog-api-key`, context under `input` as text, no routing preferences.
    Gemini,
}

impl DecisionsWire {
    /// The wire a backend kind speaks, or `None` for a kind with no Decisions route.
    pub fn for_kind(kind: crate::credentials::ProviderKind) -> Option<Self> {
        match kind {
            crate::credentials::ProviderKind::OpenRouter => Some(Self::OpenRouter),
            crate::credentials::ProviderKind::Gemini => Some(Self::Gemini),
            _ => None,
        }
    }

    /// The endpoint for a backend of this wire. A Gemini backend with its own `base_url`
    /// (a host root, as its completions use) gets `/v1beta/decisions` under it.
    pub fn url(self, base_url: Option<&str>) -> String {
        match (self, base_url) {
            (Self::Gemini, Some(base)) => {
                format!("{}/v1beta/decisions", base.trim_end_matches('/'))
            }
            (Self::Gemini, None) => GEMINI_DECISIONS_URL.to_string(),
            (Self::OpenRouter, _) => OPENROUTER_DECISIONS_URL.to_string(),
        }
    }
}

/// A `choice` may name at most this many options.
pub const MAX_CHOICE_OPTIONS: usize = 255;

/// A `score` scale has 2 to this many levels.
pub const MAX_SCORE_LEVELS: usize = 10;

/// The kind of answer a question asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum QuestionKind {
    /// Pick one label. `criteria` is an object from label to what it means.
    Choice,
    /// Rate on an ordered scale. `criteria` is an array of level names, lowest first.
    Score,
    /// Yes or no. The answer is the probability of yes. Takes no `criteria`.
    Noul,
}

/// What a `choice` or `score` question's `criteria` holds.
#[derive(Debug, Clone, PartialEq, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum Criteria {
    /// `choice`: label to its meaning. Labels are sent sorted.
    Options(BTreeMap<String, String>),
    /// `score`: level names, lowest first.
    Levels(Vec<String>),
}

/// Hand-written, as `RawSlot` is in `config.rs`: an untagged derive reduces a bad shape
/// to "did not match any variant", naming neither the bad value nor the fix.
impl<'de> Deserialize<'de> for Criteria {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        match serde_json::Value::deserialize(d)? {
            v @ serde_json::Value::Object(_) => {
                serde_json::from_value(v).map(Self::Options).map_err(|e| {
                    D::Error::custom(format!(
                        "`criteria` as an object maps each option label to a string \
                         meaning ({e}); use it for a `choice`"
                    ))
                })
            }
            v @ serde_json::Value::Array(_) => {
                serde_json::from_value(v).map(Self::Levels).map_err(|e| {
                    D::Error::custom(format!(
                        "`criteria` as an array lists level names as strings ({e}); use \
                         it for a `score`"
                    ))
                })
            }
            other => Err(D::Error::custom(format!(
                "`criteria` is an object from option label to meaning (for `choice`), an \
                 array of level names (for `score`), or, on a Gemini cast, an object with \
                 `true_description` and `false_description` (for `noul`), got {other}"
            ))),
        }
    }
}

/// One typed question. Its id is the key it sits under in the request's map.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Question {
    /// `choice`, `score`, or `noul`.
    #[serde(rename = "type")]
    pub kind: QuestionKind,
    /// The question, as the model reads it.
    pub instructions: String,
    /// `choice`: an object from option label to what it means (2 to 255 options; include
    /// an `other` option). `score`: an array of 2 to 10 level names, lowest first. `noul`:
    /// omit, or on a Gemini cast an object with `true_description` and/or
    /// `false_description` saying what each answer means.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub criteria: Option<Criteria>,
}

impl Question {
    /// Check the question against the rules the provider does not enforce itself, naming
    /// `id` in the refusal.
    pub fn validate(&self, id: &str, wire: DecisionsWire) -> Result<(), DecideError> {
        let bad = |why: String| DecideError::InvalidQuestion {
            id: id.to_string(),
            why,
        };
        if self.instructions.trim().is_empty() {
            return Err(bad("`instructions` is empty; say what to decide".into()));
        }
        match (self.kind, &self.criteria) {
            (QuestionKind::Choice, Some(Criteria::Options(o))) => {
                if o.len() < 2 || o.len() > MAX_CHOICE_OPTIONS {
                    return Err(bad(format!(
                        "a `choice` needs 2 to {MAX_CHOICE_OPTIONS} options in `criteria`, got {}; \
                         with one option the answer is always 1.0",
                        o.len()
                    )));
                }
                if let Some((label, _)) = o.iter().find(|(l, _)| l.trim().is_empty()) {
                    return Err(bad(format!("option label {label:?} is empty")));
                }
                Ok(())
            }
            (QuestionKind::Choice, _) => Err(bad(
                "a `choice` takes `criteria` as an object from option label to its meaning, \
                 e.g. {\"safe\": \"read-only\", \"risky\": \"changes something\", \"other\": \
                 \"anything else\"}"
                    .into(),
            )),
            (QuestionKind::Score, Some(Criteria::Levels(l))) => {
                if l.len() < 2 || l.len() > MAX_SCORE_LEVELS {
                    return Err(bad(format!(
                        "a `score` needs 2 to {MAX_SCORE_LEVELS} levels in `criteria`, got {}",
                        l.len()
                    )));
                }
                if l.iter().any(|n| n.trim().is_empty()) {
                    return Err(bad("a level name is empty".into()));
                }
                Ok(())
            }
            (QuestionKind::Score, _) => Err(bad(
                "a `score` takes `criteria` as an array of level names, lowest first, \
                 e.g. [\"easy\", \"hard\", \"impossible\"]"
                    .into(),
            )),
            (QuestionKind::Noul, None) => Ok(()),
            // Gemini reads these: a probe (2026-11-12) that swapped the two descriptions
            // flipped its answer from 0.97 to 0.03. Jev on OpenRouter accepted the same
            // criteria and answered 0.97 both ways, so there they are refused rather than
            // silently ignored.
            (QuestionKind::Noul, Some(Criteria::Options(o))) if wire == DecisionsWire::Gemini => {
                const KEYS: [&str; 2] = ["true_description", "false_description"];
                if o.is_empty() {
                    return Err(bad(
                        "`criteria` on a `noul` is empty; omit it, or give \
                         `true_description` and/or `false_description`"
                            .into(),
                    ));
                }
                if let Some(k) = o.keys().find(|k| !KEYS.contains(&k.as_str())) {
                    return Err(bad(format!(
                        "a `noul` takes only `true_description` and `false_description` in \
                         `criteria`, got {k:?}"
                    )));
                }
                if o.values().any(|v| v.trim().is_empty()) {
                    return Err(bad("a `noul` description is empty".into()));
                }
                Ok(())
            }
            (QuestionKind::Noul, Some(Criteria::Options(_))) => Err(bad(
                "this cast's decision model ignores `noul` criteria, so they are refused; \
                 put what yes means in `instructions`, or use a cast whose `decision` slot \
                 is on a `gemini` backend"
                    .into(),
            )),
            (QuestionKind::Noul, Some(Criteria::Levels(_))) => Err(bad(
                "a `noul` takes no list in `criteria`; put the statement in `instructions`, \
                 and on a Gemini cast describe the answers with an object holding \
                 `true_description` and `false_description`"
                    .into(),
            )),
        }
    }
}

/// A validated request: state plus questions, for one model, on one wire.
#[derive(Debug, Clone)]
pub struct DecisionRequest {
    model: String,
    state: serde_json::Value,
    questions: BTreeMap<String, Question>,
    provider: Option<ProviderPrefs>,
    wire: DecisionsWire,
}

/// The `provider` routing preferences OpenRouter accepts on a decisions request. A live
/// probe (2026-10-06) found it validates `data_collection` as `deny` or `allow` and
/// rejects any other value or shape, so a sent preference is read, not ignored.
#[derive(Debug, Clone, Serialize)]
struct ProviderPrefs {
    data_collection: &'static str,
}

impl DecisionRequest {
    /// Build an OpenRouter request, refusing an empty question map or any invalid
    /// question. [`new_for`](Self::new_for) builds one for either wire.
    pub fn new(
        model: impl Into<String>,
        state: serde_json::Value,
        questions: BTreeMap<String, Question>,
    ) -> Result<Self, DecideError> {
        Self::new_for(DecisionsWire::OpenRouter, model, state, questions)
    }

    /// Build a request for `wire`, validating each question by that wire's rules. The
    /// wire is fixed here, so a request cannot change wires after it was checked.
    pub fn new_for(
        wire: DecisionsWire,
        model: impl Into<String>,
        state: serde_json::Value,
        questions: BTreeMap<String, Question>,
    ) -> Result<Self, DecideError> {
        if questions.is_empty() {
            return Err(DecideError::NoQuestions);
        }
        for (id, q) in &questions {
            q.validate(id, wire)?;
        }
        Ok(Self {
            model: model.into(),
            state,
            questions,
            provider: None,
            wire,
        })
    }

    pub fn wire(&self) -> DecisionsWire {
        self.wire
    }

    /// The JSON body for this request's wire. Gemini reads its context as text, so a
    /// JSON `state` is sent as its JSON text and a string `state` as itself; it has no
    /// routing preferences, so `provider` is OpenRouter's alone.
    pub fn body(&self) -> serde_json::Value {
        match self.wire {
            DecisionsWire::OpenRouter => {
                let mut v = serde_json::json!({
                    "model": self.model,
                    "state": self.state,
                    "questions": self.questions,
                });
                if let Some(p) = &self.provider {
                    v["provider"] = serde_json::json!(p);
                }
                v
            }
            DecisionsWire::Gemini => {
                let input = match &self.state {
                    serde_json::Value::String(s) => s.clone(),
                    other => serde_json::to_string_pretty(other).unwrap_or_default(),
                };
                serde_json::json!({
                    "model": self.model,
                    "input": input,
                    "questions": self.questions,
                })
            }
        }
    }

    /// Route only to hosts that do not retain or train on the state
    /// (`provider.data_collection = "deny"`), the same pin OpenRouter completions carry
    /// for a backend whose `data_collection` is `deny`.
    pub fn deny_data_collection(mut self) -> Self {
        self.provider = Some(ProviderPrefs {
            data_collection: "deny",
        });
        self
    }

    pub fn model(&self) -> &str {
        &self.model
    }
}

/// One answer, shaped by its question's kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    Choice {
        choice: String,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
    Score {
        score: f64,
        legend: BTreeMap<String, String>,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
    Noul {
        noul: f64,
        /// Gemini reports it (`max(noul, 1 - noul)`); OpenRouter does not.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        confidence: Option<f64>,
    },
}

/// Token counts and cost as the provider reports them. Gemini spells the counts
/// `total_input_tokens` and `total_output_tokens` and reports no cost. Read through
/// [`RawUsage`] rather than serde aliases: an alias is the same field under two names, so a
/// reply carrying both spellings would fail as a duplicate field.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(from = "RawUsage")]
pub struct DecisionUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// USD, when the gateway reports it.
    #[serde(default)]
    pub cost: Option<f64>,
}

/// Both providers' spellings, each optional; the plain name wins when both are present.
#[derive(Deserialize)]
struct RawUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    total_input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    total_output_tokens: Option<u64>,
    #[serde(default)]
    cost: Option<f64>,
}

impl From<RawUsage> for DecisionUsage {
    fn from(r: RawUsage) -> Self {
        Self {
            input_tokens: r.input_tokens.or(r.total_input_tokens).unwrap_or(0),
            output_tokens: r.output_tokens.or(r.total_output_tokens).unwrap_or(0),
            cost: r.cost,
        }
    }
}

impl DecisionUsage {
    /// One footer line: tokens and the cost when the gateway reported them, `None` when
    /// it reported nothing (a zero line would claim a free call).
    pub fn footer(&self) -> Option<String> {
        if self.input_tokens == 0 && self.output_tokens == 0 && self.cost.is_none() {
            return None;
        }
        let mut line = format!(
            "tokens · {} in · {} out",
            self.input_tokens, self.output_tokens
        );
        if let Some(cost) = self.cost {
            line.push_str(&format!(" · ${cost:.6}"));
        }
        Some(line)
    }
}

/// The provider's answer, checked against the request that produced it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionResponse {
    /// The versioned model that answered, e.g. `typesafe/jev-1.13-20260917`. Gemini's
    /// reply names none, so the client fills in the model asked for.
    #[serde(default)]
    pub model: String,
    pub answers: BTreeMap<String, Answer>,
    #[serde(default)]
    pub usage: DecisionUsage,
    #[serde(default)]
    pub id: Option<String>,
}

impl DecisionResponse {
    /// Check that every question has an answer of its own kind, and that a `choice`
    /// names one of its options. A mismatch is an error, never a repaired value.
    pub fn check_against(&self, req: &DecisionRequest) -> Result<(), DecideError> {
        let mismatch = |id: &str, why: String| DecideError::Mismatch {
            id: id.to_string(),
            why,
        };
        for (id, q) in &req.questions {
            let Some(a) = self.answers.get(id) else {
                return Err(mismatch(id, "the response has no answer for it".into()));
            };
            match (q.kind, a, &q.criteria) {
                (
                    QuestionKind::Choice,
                    Answer::Choice { choice, .. },
                    Some(Criteria::Options(o)),
                ) => {
                    if !o.contains_key(choice) {
                        return Err(mismatch(
                            id,
                            format!("the chosen label {choice:?} is not one of the options"),
                        ));
                    }
                }
                (QuestionKind::Score, Answer::Score { legend, .. }, Some(Criteria::Levels(l))) => {
                    let echoed =
                        l.iter().enumerate().all(|(i, name)| {
                            legend.get(&i.to_string()).is_some_and(|got| got == name)
                        }) && legend.len() == l.len();
                    if !echoed {
                        return Err(mismatch(
                            id,
                            format!(
                                "the answer's legend {legend:?} is not the levels asked, {l:?}"
                            ),
                        ));
                    }
                }
                (QuestionKind::Noul, Answer::Noul { noul, .. }, _) => {
                    if !(0.0..=1.0).contains(noul) {
                        return Err(mismatch(
                            id,
                            format!("a noul answer must be a probability from 0 to 1, got {noul}"),
                        ));
                    }
                }
                (_, other, _) => {
                    return Err(mismatch(
                        id,
                        format!("asked for a {:?} answer, got {}", q.kind, kind_of(other)),
                    ));
                }
            }
        }
        if let Some(extra) = self
            .answers
            .keys()
            .find(|k| !req.questions.contains_key(*k))
        {
            return Err(mismatch(
                extra,
                "the response answers a question that was not asked".into(),
            ));
        }
        Ok(())
    }
}

fn kind_of(a: &Answer) -> &'static str {
    match a {
        Answer::Choice { .. } => "choice",
        Answer::Score { .. } => "score",
        Answer::Noul { .. } => "noul",
    }
}

/// Why a decision did not return answers.
#[derive(Debug, thiserror::Error)]
pub enum DecideError {
    #[error("no questions: send at least one question under `questions`, as an object from question id to {{type, instructions, criteria}}")]
    NoQuestions,
    #[error("question {id:?} is invalid: {why}")]
    InvalidQuestion { id: String, why: String },
    /// The provider refused or failed the request. `message` is its own text, verbatim.
    #[error("the decisions provider answered HTTP {status}: {message}")]
    Provider { status: u16, message: String },
    #[error("the provider's response does not match question {id:?}: {why}")]
    Mismatch { id: String, why: String },
    #[error("the provider's response could not be read: {0}")]
    Unreadable(String),
    #[error("the decisions request failed before an answer arrived: {0}")]
    Transport(String),
}

impl DecideError {
    /// What the caller can do next, from the failure's class: a 429, a 5xx, or a transport
    /// failure may pass on a retry; any other refusal needs a different request.
    pub fn next_step(&self) -> &'static str {
        match self {
            Self::Provider { status, .. } if *status == 429 || *status >= 500 => {
                "The provider was busy or down; send the same request again in a moment."
            }
            Self::Transport(_) => {
                "No answer arrived; send the same request again, and check the network if it repeats."
            }
            Self::Provider { .. } => {
                "The provider refused this request; change the request, not the timing."
            }
            Self::Mismatch { .. } | Self::Unreadable(_) => {
                "The provider's reply was not usable; send the request again, and use `consult` if it repeats."
            }
            Self::NoQuestions | Self::InvalidQuestion { .. } => {
                "Fix the question and send it again."
            }
        }
    }
}

/// Pull the human message out of the provider's `{"error": {"message", "code"}}` body,
/// falling back to the raw body so nothing the provider said is dropped.
fn provider_message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| body.to_string())
}

/// A client for one Decisions endpoint and key. The request's wire picks the key
/// header.
pub struct DecisionClient {
    url: String,
    key: String,
    http: reqwest::Client,
}

impl DecisionClient {
    /// `url` is the full endpoint. Production passes [`DecisionsWire::url`]; a test
    /// passes a local listener.
    pub fn new(
        url: impl Into<String>,
        key: impl Into<String>,
        timeout: Duration,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            url: url.into(),
            key: key.into(),
            http: crate::tls::https_client(timeout).context("decisions http client")?,
        })
    }

    /// Send the request and return the checked response.
    pub async fn decide(&self, req: &DecisionRequest) -> Result<DecisionResponse, DecideError> {
        let post = self.http.post(&self.url);
        let post = match req.wire {
            DecisionsWire::OpenRouter => post.bearer_auth(&self.key),
            DecisionsWire::Gemini => post.header("x-goog-api-key", &self.key),
        };
        let resp = post
            .json(&req.body())
            .send()
            .await
            .map_err(|e| DecideError::Transport(e.to_string()))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| DecideError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(DecideError::Provider {
                status: status.as_u16(),
                message: provider_message(&body),
            });
        }
        let mut parsed: DecisionResponse =
            serde_json::from_str(&body).map_err(|e| DecideError::Unreadable(e.to_string()))?;
        if parsed.model.is_empty() {
            parsed.model = req.model.clone();
        }
        parsed.check_against(req)?;
        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(pairs: &[(&str, &str)]) -> Criteria {
        Criteria::Options(
            pairs
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
        )
    }

    fn choice(o: Criteria) -> Question {
        Question {
            kind: QuestionKind::Choice,
            instructions: "which".into(),
            criteria: Some(o),
        }
    }

    fn qs(id: &str, q: Question) -> BTreeMap<String, Question> {
        BTreeMap::from([(id.to_string(), q)])
    }

    #[test]
    fn a_single_option_choice_is_refused_before_any_request() {
        let q = choice(opts(&[("only", "x")]));
        let err = DecisionRequest::new("m", "s".into(), qs("q", q)).unwrap_err();
        assert!(matches!(err, DecideError::InvalidQuestion { .. }), "{err}");
        assert!(err.to_string().contains("2 to 255"), "{err}");
    }

    #[test]
    fn choice_without_criteria_or_with_levels_is_refused() {
        let none = Question {
            criteria: None,
            ..choice(opts(&[]))
        };
        assert!(DecisionRequest::new("m", "s".into(), qs("q", none)).is_err());
        let levels = choice(Criteria::Levels(vec!["a".into(), "b".into()]));
        assert!(DecisionRequest::new("m", "s".into(), qs("q", levels)).is_err());
    }

    #[test]
    fn score_levels_must_be_2_to_10() {
        let mk = |n: usize| Question {
            kind: QuestionKind::Score,
            instructions: "how".into(),
            criteria: Some(Criteria::Levels((0..n).map(|i| i.to_string()).collect())),
        };
        assert!(DecisionRequest::new("m", "s".into(), qs("q", mk(1))).is_err());
        assert!(DecisionRequest::new("m", "s".into(), qs("q", mk(2))).is_ok());
        assert!(DecisionRequest::new("m", "s".into(), qs("q", mk(10))).is_ok());
        assert!(DecisionRequest::new("m", "s".into(), qs("q", mk(11))).is_err());
    }

    #[test]
    fn noul_criteria_are_gemini_only() {
        let noul = |c: Option<Criteria>| Question {
            kind: QuestionKind::Noul,
            instructions: "is it".into(),
            criteria: c,
        };
        let on = |wire, q: Question| DecisionRequest::new_for(wire, "m", "s".into(), qs("q", q));
        let both = opts(&[("true_description", "yes means x"), ("false_description", "no means y")]);
        let one = opts(&[("true_description", "yes means x")]);
        for wire in [DecisionsWire::OpenRouter, DecisionsWire::Gemini] {
            assert!(on(wire, noul(None)).is_ok());
            let levels = Criteria::Levels(vec!["a".into(), "b".into()]);
            assert!(on(wire, noul(Some(levels))).is_err());
        }
        // Gemini reads the descriptions; one alone is enough.
        let req = on(DecisionsWire::Gemini, noul(Some(both.clone()))).unwrap();
        assert_eq!(
            req.body()["questions"]["q"]["criteria"],
            serde_json::json!({"true_description": "yes means x", "false_description": "no means y"})
        );
        assert!(on(DecisionsWire::Gemini, noul(Some(one))).is_ok());
        // Jev ignores them, so OpenRouter refuses them and names the way out.
        let err = on(DecisionsWire::OpenRouter, noul(Some(both))).unwrap_err();
        assert!(err.to_string().contains("`gemini` backend"), "{err}");
        // A key Gemini would refuse, an empty object, and an empty description are caught here.
        for bad in [
            opts(&[("yes", "y")]),
            opts(&[]),
            opts(&[("true_description", " ")]),
        ] {
            assert!(on(DecisionsWire::Gemini, noul(Some(bad))).is_err());
        }
    }

    #[test]
    fn usage_reads_either_spelling_and_survives_both() {
        let u: DecisionUsage =
            serde_json::from_str(r#"{"input_tokens":5,"total_input_tokens":7,"output_tokens":1}"#)
                .unwrap();
        assert_eq!((u.input_tokens, u.output_tokens), (5, 1));
        let g: DecisionUsage =
            serde_json::from_str(r#"{"total_input_tokens":7,"total_output_tokens":0,"total_tokens":7}"#)
                .unwrap();
        assert_eq!((g.input_tokens, g.output_tokens, g.cost), (7, 0, None));
    }

    #[test]
    fn empty_questions_and_empty_instructions_are_refused() {
        assert!(matches!(
            DecisionRequest::new("m", "s".into(), BTreeMap::new()),
            Err(DecideError::NoQuestions)
        ));
        let q = Question {
            kind: QuestionKind::Noul,
            instructions: "  ".into(),
            criteria: None,
        };
        assert!(DecisionRequest::new("m", "s".into(), qs("q", q)).is_err());
    }

    #[test]
    fn request_serializes_to_the_probed_wire_shape() {
        let q = choice(opts(&[("a", "first"), ("b", "second")]));
        let req = DecisionRequest::new(
            "typesafe/jev-1.13",
            serde_json::json!({"cmd": "ls"}),
            qs("v", q),
        )
        .unwrap();
        let v = req.body();
        assert_eq!(
            v,
            serde_json::json!({
                "model": "typesafe/jev-1.13",
                "state": {"cmd": "ls"},
                "questions": {"v": {"type": "choice", "instructions": "which",
                                    "criteria": {"a": "first", "b": "second"}}}
            })
        );
    }

    /// The reply recorded from the live probe of 2026-10-06, trimmed to one of each kind.
    const PROBED: &str = r#"{"model":"typesafe/jev-1.13-20260917","answers":{
      "verdict":{"type":"choice","choice":"risky","probabilities":{"safe":0,"other":0,"risky":1},"confidence":1},
      "undo":{"type":"score","score":1.1,"legend":{"0":"easy","1":"hard","2":"impossible"},"probabilities":{"0":0.07,"1":0.76,"2":0.17},"confidence":0.64},
      "pushes":{"type":"noul","noul":0.96}},
      "usage":{"input_tokens":399,"output_tokens":72,"cost":0.000016758},
      "id":"gen-dec-1","provider":"TypeSafe"}"#;

    fn probed_request() -> DecisionRequest {
        probed_request_for(DecisionsWire::OpenRouter)
    }

    fn probed_request_for(wire: DecisionsWire) -> DecisionRequest {
        let mut m = qs(
            "verdict",
            choice(opts(&[("safe", "s"), ("risky", "r"), ("other", "o")])),
        );
        m.insert(
            "undo".into(),
            Question {
                kind: QuestionKind::Score,
                instructions: "u".into(),
                criteria: Some(Criteria::Levels(vec![
                    "easy".into(),
                    "hard".into(),
                    "impossible".into(),
                ])),
            },
        );
        m.insert(
            "pushes".into(),
            Question {
                kind: QuestionKind::Noul,
                instructions: "p".into(),
                criteria: None,
            },
        );
        DecisionRequest::new_for(wire, "typesafe/jev-1.13", "x".into(), m).unwrap()
    }

    #[test]
    fn the_probed_response_parses_and_checks() {
        let r: DecisionResponse = serde_json::from_str(PROBED).unwrap();
        assert_eq!(r.usage.cost, Some(0.000016758));
        r.check_against(&probed_request()).unwrap();
    }

    #[test]
    fn a_missing_answer_is_an_error_naming_the_question() {
        let mut r: DecisionResponse = serde_json::from_str(PROBED).unwrap();
        r.answers.remove("undo");
        let err = r.check_against(&probed_request()).unwrap_err();
        assert!(err.to_string().contains("\"undo\""), "{err}");
    }

    #[test]
    fn a_wrong_kind_of_answer_is_an_error() {
        let mut r: DecisionResponse = serde_json::from_str(PROBED).unwrap();
        r.answers
            .insert("pushes".into(), Answer::Noul { noul: 0.5, confidence: None });
        r.answers
            .insert("verdict".into(), Answer::Noul { noul: 0.5, confidence: None });
        let err = r.check_against(&probed_request()).unwrap_err();
        assert!(err.to_string().contains("verdict"), "{err}");
    }

    #[test]
    fn a_choice_outside_the_options_is_an_error() {
        let mut r: DecisionResponse = serde_json::from_str(PROBED).unwrap();
        if let Some(Answer::Choice { choice, .. }) = r.answers.get_mut("verdict") {
            *choice = "invented".into();
        }
        let err = r.check_against(&probed_request()).unwrap_err();
        assert!(err.to_string().contains("invented"), "{err}");
    }

    #[test]
    fn an_unrequested_answer_is_an_error() {
        let mut r: DecisionResponse = serde_json::from_str(PROBED).unwrap();
        r.answers.insert("extra".into(), Answer::Noul { noul: 0.1, confidence: None });
        assert!(r.check_against(&probed_request()).is_err());
    }

    #[test]
    fn provider_error_message_is_extracted_or_kept_raw() {
        assert_eq!(
            provider_message(
                r#"{"error":{"message":"Model typesafe/nope does not exist","code":400}}"#
            ),
            "Model typesafe/nope does not exist"
        );
        assert_eq!(
            provider_message("<html>bad gateway</html>"),
            "<html>bad gateway</html>"
        );
    }

    #[test]
    fn a_legend_that_is_not_the_levels_asked_is_an_error() {
        let mut r: DecisionResponse = serde_json::from_str(PROBED).unwrap();
        if let Some(Answer::Score { legend, .. }) = r.answers.get_mut("undo") {
            legend.insert("1".into(), "medium".into());
        }
        let err = r.check_against(&probed_request()).unwrap_err();
        assert!(err.to_string().contains("legend"), "{err}");
    }

    #[test]
    fn a_noul_outside_zero_to_one_is_an_error() {
        let mut r: DecisionResponse = serde_json::from_str(PROBED).unwrap();
        r.answers
            .insert("pushes".into(), Answer::Noul { noul: 1.5, confidence: None });
        let err = r.check_against(&probed_request()).unwrap_err();
        assert!(err.to_string().contains("pushes"), "{err}");
    }

    #[test]
    fn empty_level_names_are_refused() {
        let q = Question {
            kind: QuestionKind::Score,
            instructions: "how".into(),
            criteria: Some(Criteria::Levels(vec!["".into(), "hard".into()])),
        };
        assert!(DecisionRequest::new("m", "s".into(), qs("q", q)).is_err());
    }

    #[test]
    fn a_bad_criteria_shape_names_the_fix() {
        let err = serde_json::from_value::<Question>(
            serde_json::json!({"type": "choice", "instructions": "x", "criteria": {"a": 1}}),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("string meaning") && err.contains("`choice`"),
            "{err}"
        );
        let err = serde_json::from_value::<Question>(
            serde_json::json!({"type": "score", "instructions": "x", "criteria": "easy,hard"}),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("object from option label") && err.contains("array"),
            "{err}"
        );
    }

    #[test]
    fn data_collection_deny_serializes_as_the_probed_provider_field() {
        let q = qs(
            "q",
            Question {
                kind: QuestionKind::Noul,
                instructions: "x".into(),
                criteria: None,
            },
        );
        let plain = DecisionRequest::new("m", "s".into(), q.clone()).unwrap().body();
        assert!(plain.get("provider").is_none(), "{plain}");
        let denied = DecisionRequest::new("m", "s".into(), q.clone())
            .unwrap()
            .deny_data_collection()
            .body();
        assert_eq!(
            denied["provider"],
            serde_json::json!({"data_collection": "deny"})
        );
        // Gemini has no routing preferences; the pin is OpenRouter's alone.
        let gemini = DecisionRequest::new_for(DecisionsWire::Gemini, "m", "s".into(), q)
            .unwrap()
            .deny_data_collection()
            .body();
        assert!(gemini.get("provider").is_none(), "{gemini}");
    }

    #[test]
    fn a_gemini_request_sends_the_state_as_input_text() {
        let q = choice(opts(&[("a", "first"), ("b", "second")]));
        let json_state = DecisionRequest::new_for(
            DecisionsWire::Gemini,
            "gemini-decision-1",
            serde_json::json!({"cmd": "ls"}),
            qs("v", q.clone()),
        )
        .unwrap()
        .body();
        assert_eq!(
            json_state,
            serde_json::json!({
                "model": "gemini-decision-1",
                "input": "{\n  \"cmd\": \"ls\"\n}",
                "questions": {"v": {"type": "choice", "instructions": "which",
                                    "criteria": {"a": "first", "b": "second"}}}
            })
        );
        assert!(json_state.get("state").is_none());
        let text_state =
            DecisionRequest::new_for(DecisionsWire::Gemini, "gemini-decision-1", "plain text".into(), qs("v", q))
                .unwrap()
                .body();
        assert_eq!(text_state["input"], "plain text");
    }

    /// The reply recorded from the live Gemini probe of 2026-11-12, one of each kind.
    const GEMINI_PROBED: &str = r#"{"answers":{
      "pushes":{"confidence":0.98,"noul":0.98,"type":"noul"},
      "undo":{"confidence":0.73,"score":0.30,"probabilities":{"0":0.73,"1":0.23,"2":0.04},"legend":{"2":"impossible","1":"hard","0":"easy"},"type":"score"},
      "verdict":{"confidence":0.89,"choice":"risky","probabilities":{"safe":0.06,"other":0.05,"risky":0.89},"type":"choice"}},
      "usage":{"total_tokens":295,"total_input_tokens":295,"input_tokens_by_modality":[{"modality":"text","tokens":295}],"total_output_tokens":0}}"#;

    #[test]
    fn the_gemini_probed_response_parses_and_checks() {
        let r: DecisionResponse = serde_json::from_str(GEMINI_PROBED).unwrap();
        assert_eq!(r.model, "", "Gemini names no model; the client fills it in");
        assert_eq!((r.usage.input_tokens, r.usage.output_tokens, r.usage.cost), (295, 0, None));
        assert_eq!(
            r.answers["pushes"],
            Answer::Noul { noul: 0.98, confidence: Some(0.98) }
        );
        r.check_against(&probed_request_for(DecisionsWire::Gemini)).unwrap();
        assert_eq!(r.usage.footer().as_deref(), Some("tokens · 295 in · 0 out"));
    }

    #[test]
    fn each_wire_has_its_endpoint() {
        assert_eq!(DecisionsWire::OpenRouter.url(None), OPENROUTER_DECISIONS_URL);
        assert_eq!(DecisionsWire::Gemini.url(None), GEMINI_DECISIONS_URL);
        assert_eq!(
            DecisionsWire::Gemini.url(Some("https://proxy.test/")),
            "https://proxy.test/v1beta/decisions"
        );
    }

    #[test]
    fn unreported_usage_has_no_footer() {
        assert_eq!(DecisionUsage::default().footer(), None);
        let u = DecisionUsage {
            input_tokens: 5,
            output_tokens: 2,
            cost: Some(0.000001),
        };
        assert_eq!(u.footer().unwrap(), "tokens · 5 in · 2 out · $0.000001");
    }

    #[test]
    fn next_step_separates_retry_from_change_the_request() {
        let busy = DecideError::Provider {
            status: 429,
            message: "x".into(),
        };
        let bad = DecideError::Provider {
            status: 400,
            message: "x".into(),
        };
        assert!(busy.next_step().contains("again"), "{}", busy.next_step());
        assert!(
            bad.next_step().contains("change the request"),
            "{}",
            bad.next_step()
        );
    }
}
