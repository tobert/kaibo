//! Live probes of the two Decisions APIs through `DecisionClient`. Ignored by default:
//! each spends a fraction of a cent. OpenRouter's needs `~/.openrouter-key.txt` (or
//! `OPENROUTER_API_KEY`); Gemini's needs `~/.gemini-api-key` (or `GEMINI_API_KEY`). Run
//! with `cargo test --test decide_live -- --ignored`.

use std::collections::BTreeMap;
use std::time::Duration;

use kaibo::decide::{
    Answer, Criteria, DecisionClient, DecisionRequest, DecisionsWire, Question, QuestionKind,
    GEMINI_DECISIONS_URL, OPENROUTER_DECISIONS_URL,
};

fn key_from(env: &str, file: &str) -> String {
    if let Ok(k) = std::env::var(env) {
        return k;
    }
    let home = std::env::var("HOME").expect("HOME");
    std::fs::read_to_string(format!("{home}/{file}"))
        .unwrap_or_else(|_| panic!("{env} or ~/{file}"))
        .trim()
        .to_string()
}

/// One of each question kind about a force-push, the probe both routes answer.
fn questions() -> BTreeMap<String, Question> {
    BTreeMap::from([
        (
            "verdict".to_string(),
            Question {
                kind: QuestionKind::Choice,
                instructions:
                    "How risky is this shell command for a coding agent to run unattended".into(),
                criteria: Some(Criteria::Options(BTreeMap::from([
                    (
                        "safe".to_string(),
                        "local, reversible, read-only".to_string(),
                    ),
                    (
                        "risky".to_string(),
                        "outward-facing or hard to undo".to_string(),
                    ),
                    ("other".to_string(), "anything else".to_string()),
                ]))),
            },
        ),
        (
            "undo".to_string(),
            Question {
                kind: QuestionKind::Score,
                instructions: "How hard is it to take back".into(),
                criteria: Some(Criteria::Levels(vec![
                    "easy".into(),
                    "hard".into(),
                    "impossible".into(),
                ])),
            },
        ),
        (
            "pushes".to_string(),
            Question {
                kind: QuestionKind::Noul,
                instructions: "The command publishes to a remote".into(),
                criteria: None,
            },
        ),
    ])
}

#[tokio::test]
#[ignore = "live: spends a fraction of a cent against OpenRouter"]
async fn jev_answers_all_three_question_kinds() {
    kaibo::tls::ensure_crypto_provider();
    let req = DecisionRequest::new(
        "typesafe/jev-1.13",
        "git push --force origin main".into(),
        questions(),
    )
    .unwrap()
    .deny_data_collection();
    let key = key_from("OPENROUTER_API_KEY", ".openrouter-key.txt");
    let client =
        DecisionClient::new(OPENROUTER_DECISIONS_URL, key, Duration::from_secs(30)).unwrap();
    let resp = client.decide(&req).await.expect("a live decision");

    match &resp.answers["verdict"] {
        Answer::Choice { choice, .. } => assert_eq!(choice, "risky", "{resp:?}"),
        other => panic!("verdict should be a choice: {other:?}"),
    }
    assert!(
        matches!(&resp.answers["pushes"], Answer::Noul { noul, .. } if *noul > 0.5),
        "{resp:?}"
    );
    assert!(
        resp.usage.cost.is_some(),
        "OpenRouter reports usage.cost: {resp:?}"
    );
}

#[tokio::test]
#[ignore = "live: spends a fraction of a cent against Google's Gemini API"]
async fn gemini_decision_answers_all_three_question_kinds() {
    kaibo::tls::ensure_crypto_provider();
    let req = DecisionRequest::new_for(
        DecisionsWire::Gemini,
        "gemini-decision-1",
        "git push --force origin main".into(),
        questions(),
    )
    .unwrap();
    let key = key_from("GEMINI_API_KEY", ".gemini-api-key");
    let client = DecisionClient::new(GEMINI_DECISIONS_URL, key, Duration::from_secs(30)).unwrap();
    let resp = client.decide(&req).await.expect("a live decision");

    assert_eq!(resp.model, "gemini-decision-1", "the client names the model asked");
    match &resp.answers["verdict"] {
        Answer::Choice { choice, .. } => assert_eq!(choice, "risky", "{resp:?}"),
        other => panic!("verdict should be a choice: {other:?}"),
    }
    assert!(
        matches!(&resp.answers["pushes"], Answer::Noul { noul, confidence: Some(_) } if *noul > 0.5),
        "Gemini reports a noul with its confidence: {resp:?}"
    );
    assert!(resp.usage.input_tokens > 0, "{resp:?}");
}

/// Gemini reads `noul` criteria: describing "true" as the opposite of the statement flips
/// the answer (0.97 to 0.03 in the 2026-11-12 probe).
#[tokio::test]
#[ignore = "live: spends a fraction of a cent against Google's Gemini API"]
async fn gemini_reads_noul_criteria() {
    kaibo::tls::ensure_crypto_provider();
    let swapped = Question {
        kind: QuestionKind::Noul,
        instructions: "Does the command publish to a remote?".into(),
        criteria: Some(Criteria::Options(BTreeMap::from([
            ("true_description".to_string(), "it stays on this machine".to_string()),
            ("false_description".to_string(), "it sends commits to a remote server".to_string()),
        ]))),
    };
    let req = DecisionRequest::new_for(
        DecisionsWire::Gemini,
        "gemini-decision-1",
        "git push --force origin main".into(),
        BTreeMap::from([("pushes".to_string(), swapped)]),
    )
    .unwrap();
    let key = key_from("GEMINI_API_KEY", ".gemini-api-key");
    let client = DecisionClient::new(GEMINI_DECISIONS_URL, key, Duration::from_secs(30)).unwrap();
    let resp = client.decide(&req).await.expect("a live decision");
    assert!(
        matches!(&resp.answers["pushes"], Answer::Noul { noul, .. } if *noul < 0.5),
        "swapped descriptions flip the answer: {resp:?}"
    );
}
