//! The Decisions client — `decide`'s wire, driven directly rather than through rig.
//!
//! A Decisions API takes application `state` plus a map of typed `questions` and returns
//! probabilities instead of text: `choice` (one label of up to 255), `score` (a level on
//! an ordered scale of 2 to 10), and `noul` (the probability a statement is true). No
//! model writes a sentence, so there is no preamble, no tool loop, and no thinking
//! budget; this is not a completion phase and does not go through `run_phase`.
//!
//! Today's route is OpenRouter's `POST /api/alpha/decisions` (model ids
//! `typesafe/jev-1.13`, `~typesafe/jev-latest`). The request and response shapes were
//! confirmed by a live probe on 2026-10-06, which also found what the provider does not
//! check, so this module checks it instead:
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Criteria {
    /// `choice`: label to its meaning, in the order the caller wrote them.
    Options(BTreeMap<String, String>),
    /// `score`: level names, lowest first.
    Levels(Vec<String>),
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
    /// omit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub criteria: Option<Criteria>,
}

impl Question {
    /// Check the question against the rules the provider does not enforce itself, naming
    /// `id` in the refusal.
    pub fn validate(&self, id: &str) -> Result<(), DecideError> {
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
                Ok(())
            }
            (QuestionKind::Score, _) => Err(bad(
                "a `score` takes `criteria` as an array of level names, lowest first, \
                 e.g. [\"easy\", \"hard\", \"impossible\"]"
                    .into(),
            )),
            (QuestionKind::Noul, None) => Ok(()),
            (QuestionKind::Noul, Some(_)) => Err(bad(
                "a `noul` takes no `criteria`; put the statement in `instructions`".into(),
            )),
        }
    }
}

/// A validated request: state plus questions, for one model.
#[derive(Debug, Clone, Serialize)]
pub struct DecisionRequest {
    model: String,
    state: serde_json::Value,
    questions: BTreeMap<String, Question>,
}

impl DecisionRequest {
    /// Build a request, refusing an empty question map or any invalid question.
    pub fn new(
        model: impl Into<String>,
        state: serde_json::Value,
        questions: BTreeMap<String, Question>,
    ) -> Result<Self, DecideError> {
        if questions.is_empty() {
            return Err(DecideError::NoQuestions);
        }
        for (id, q) in &questions {
            q.validate(id)?;
        }
        Ok(Self {
            model: model.into(),
            state,
            questions,
        })
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
    },
}

/// Token counts and cost as the provider reports them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DecisionUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    /// USD, when the gateway reports it.
    #[serde(default)]
    pub cost: Option<f64>,
}

impl DecisionUsage {
    /// One footer line: tokens, and the cost when the gateway reported it.
    pub fn footer(&self) -> String {
        let mut line = format!(
            "tokens · {} in · {} out",
            self.input_tokens, self.output_tokens
        );
        if let Some(cost) = self.cost {
            line.push_str(&format!(" · ${cost:.6}"));
        }
        line
    }
}

/// The provider's answer, checked against the request that produced it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionResponse {
    /// The versioned model that answered, e.g. `typesafe/jev-1.13-20260917`.
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
                (QuestionKind::Score, Answer::Score { .. }, _)
                | (QuestionKind::Noul, Answer::Noul { .. }, _) => {}
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

/// Pull the human message out of the provider's `{"error": {"message", "code"}}` body,
/// falling back to the raw body so nothing the provider said is dropped.
fn provider_message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| body.to_string())
}

/// A client for one Decisions endpoint and key.
pub struct DecisionClient {
    url: String,
    key: String,
    http: reqwest::Client,
}

impl DecisionClient {
    /// `url` is the full endpoint. Production passes [`OPENROUTER_DECISIONS_URL`]; a test
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
        let resp = self
            .http
            .post(&self.url)
            .bearer_auth(&self.key)
            .json(req)
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
        let parsed: DecisionResponse =
            serde_json::from_str(&body).map_err(|e| DecideError::Unreadable(e.to_string()))?;
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
    fn noul_takes_no_criteria() {
        let mut q = Question {
            kind: QuestionKind::Noul,
            instructions: "is it".into(),
            criteria: None,
        };
        assert!(DecisionRequest::new("m", "s".into(), qs("q", q.clone())).is_ok());
        q.criteria = Some(Criteria::Levels(vec!["a".into(), "b".into()]));
        assert!(DecisionRequest::new("m", "s".into(), qs("q", q)).is_err());
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
        let v = serde_json::to_value(&req).unwrap();
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
        DecisionRequest::new("typesafe/jev-1.13", "x".into(), m).unwrap()
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
            .insert("pushes".into(), Answer::Noul { noul: 0.5 });
        r.answers
            .insert("verdict".into(), Answer::Noul { noul: 0.5 });
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
        r.answers.insert("extra".into(), Answer::Noul { noul: 0.1 });
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
}
