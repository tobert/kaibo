//! A live probe of OpenRouter's Decisions API through `DecisionClient`. Ignored by
//! default: it spends a fraction of a cent and needs the key in `~/.openrouter-key.txt`
//! (or `OPENROUTER_API_KEY`). Run with `cargo test --test decide_live -- --ignored`.

use std::collections::BTreeMap;
use std::time::Duration;

use kaibo::decide::{
    Answer, Criteria, DecisionClient, DecisionRequest, Question, QuestionKind,
    OPENROUTER_DECISIONS_URL,
};

fn key() -> String {
    if let Ok(k) = std::env::var("OPENROUTER_API_KEY") {
        return k;
    }
    let home = std::env::var("HOME").expect("HOME");
    std::fs::read_to_string(format!("{home}/.openrouter-key.txt"))
        .expect("OPENROUTER_API_KEY or ~/.openrouter-key.txt")
        .trim()
        .to_string()
}

#[tokio::test]
#[ignore = "live: spends a fraction of a cent against OpenRouter"]
async fn jev_answers_all_three_question_kinds() {
    kaibo::tls::ensure_crypto_provider();
    let questions = BTreeMap::from([
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
    ]);
    let req = DecisionRequest::new(
        "typesafe/jev-1.13",
        "git push --force origin main".into(),
        questions,
    )
    .unwrap();
    let client =
        DecisionClient::new(OPENROUTER_DECISIONS_URL, key(), Duration::from_secs(30)).unwrap();
    let resp = client.decide(&req).await.expect("a live decision");

    match &resp.answers["verdict"] {
        Answer::Choice { choice, .. } => assert_eq!(choice, "risky", "{resp:?}"),
        other => panic!("verdict should be a choice: {other:?}"),
    }
    assert!(
        matches!(&resp.answers["pushes"], Answer::Noul { noul } if *noul > 0.5),
        "{resp:?}"
    );
    assert!(
        resp.usage.cost.is_some(),
        "OpenRouter reports usage.cost: {resp:?}"
    );
}
