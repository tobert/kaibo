//! The Stable Audio routes over a real socket, offline.
//!
//! The transport truths the pure-function tests cannot see: the path dialled for each
//! model family, the `Accept` header the audio routes require (`audio/*`; `image/*` is
//! outside their enum), the `model` field riding the multipart body, and — for Stable
//! Audio 3 — the job collected from `audio/results/{id}` rather than the shared
//! `results/{id}`, which documents only image content types.
//!
//! Pattern precedent: `tests/gemini_media_transport.rs`. Change the path, the Accept
//! value, or the results route and these fail.

use std::time::Duration;

use kaibo::media::{MediaModel, MediaOutcome, MediaOutput, MediaPollOutcome, MediaRequest};
use kaibo::stability::{StabilityClient, StabilityImageModel};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

struct Captured {
    head: String,
    body: Vec<u8>,
}

impl Captured {
    fn request_line(&self) -> &str {
        self.head.lines().next().unwrap_or("")
    }
    fn header(&self, name: &str) -> Option<String> {
        let want = format!("{}:", name.to_ascii_lowercase());
        self.head.lines().find_map(|l| {
            l.to_ascii_lowercase()
                .starts_with(&want)
                .then(|| l[want.len()..].trim().to_string())
        })
    }
    fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

/// One canned HTTP response: status, extra headers, body.
struct Reply {
    status: u16,
    headers: Vec<(&'static str, &'static str)>,
    body: Vec<u8>,
}

/// Serve `replies` in order, one per connection, capturing each request.
async fn serve(replies: Vec<Reply>) -> (String, tokio::sync::mpsc::UnboundedReceiver<Captured>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        for reply in replies {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let head_end = loop {
                let mut chunk = [0u8; 4096];
                let n = sock.read(&mut chunk).await.unwrap();
                assert!(n > 0, "client hung up mid-request");
                buf.extend_from_slice(&chunk[..n]);
                if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break p + 4;
                }
            };
            let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
            let len: usize = head
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse().unwrap())
                })
                .unwrap_or(0);
            while buf.len() < head_end + len {
                let mut chunk = [0u8; 4096];
                let n = sock.read(&mut chunk).await.unwrap();
                assert!(n > 0, "client hung up mid-body");
                buf.extend_from_slice(&chunk[..n]);
            }
            let mut response = format!("HTTP/1.1 {} X\r\n", reply.status);
            for (k, v) in &reply.headers {
                response.push_str(&format!("{k}: {v}\r\n"));
            }
            response.push_str(&format!(
                "content-length: {}\r\nconnection: close\r\n\r\n",
                reply.body.len()
            ));
            sock.write_all(response.as_bytes()).await.unwrap();
            sock.write_all(&reply.body).await.unwrap();
            sock.shutdown().await.ok();
            tx.send(Captured {
                head,
                body: buf[head_end..head_end + len].to_vec(),
            })
            .ok();
        }
    });
    (format!("http://{addr}"), rx)
}

fn mp3_reply() -> Reply {
    Reply {
        status: 200,
        headers: vec![
            ("content-type", "audio/mpeg"),
            ("finish-reason", "SUCCESS"),
            ("seed", "1234"),
        ],
        body: b"ID3\x03\x00\x00\x00\x00rain".to_vec(),
    }
}

fn request() -> MediaRequest {
    MediaRequest {
        prompt: "rain on a tin roof".into(),
        fields: Vec::new(),
        inputs: Vec::new(),
        op: None,
    }
}

/// Stable Audio 2.5 is synchronous: one POST to the 2.x route, asking for `audio/*`,
/// carrying `model = stable-audio-2.5`, answered with the mp3 and its seed.
#[tokio::test]
async fn stable_audio_2_5_answers_in_call_with_the_audio() {
    let (base, mut rx) = serve(vec![mp3_reply()]).await;
    let client = StabilityClient::new("k-secret", base, Duration::from_secs(5)).unwrap();
    let model = StabilityImageModel::from_parts(&client, "stable-audio-2.5", MediaOutput::Audio);

    let MediaOutcome::Complete { artifacts, .. } = model.generate(&request()).await.unwrap() else {
        panic!("the 2.x routes are synchronous")
    };
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0].mime, "audio/mpeg");
    assert_eq!(artifacts[0].seed.as_deref(), Some("1234"));
    assert_eq!(artifacts[0].bytes, b"ID3\x03\x00\x00\x00\x00rain");

    let cap = rx.recv().await.unwrap();
    assert_eq!(
        cap.request_line(),
        "POST /v2beta/audio/stable-audio-2/text-to-audio HTTP/1.1"
    );
    assert_eq!(cap.header("accept").as_deref(), Some("audio/*"));
    assert_eq!(
        cap.header("authorization").as_deref(),
        Some("Bearer k-secret")
    );
    let body = cap.body_text();
    assert!(
        body.contains("name=\"model\"\r\n\r\nstable-audio-2.5"),
        "the model field rides the form: {body}"
    );
    assert!(
        body.contains("name=\"prompt\"\r\n\r\nrain on a tin roof"),
        "{body}"
    );
    assert!(body.contains("name=\"output_format\"\r\n\r\nmp3"), "{body}");
}

/// Stable Audio 3 is deferred: the POST answers `202 {id}` on its own route, and the
/// job is collected from `audio/results/{id}` with `Accept: audio/*` — pending first,
/// then the audio.
#[tokio::test]
async fn stable_audio_3_defers_and_polls_the_audio_results_route() {
    let id = "a".repeat(64);
    let (base, mut rx) = serve(vec![
        Reply {
            status: 202,
            headers: vec![("content-type", "application/json")],
            body: format!("{{\"id\":\"{id}\"}}").into_bytes(),
        },
        Reply {
            status: 202,
            headers: vec![("content-type", "application/json")],
            body: format!("{{\"id\":\"{id}\",\"status\":\"in-progress\"}}").into_bytes(),
        },
        // What the live route sent on 2026-09-26: the audio, and neither
        // `finish-reason` nor `seed`.
        Reply {
            status: 200,
            headers: vec![("content-type", "audio/mpeg")],
            body: b"ID3\x04\x00\x00\x00\x00bell".to_vec(),
        },
    ])
    .await;
    let client = StabilityClient::new("k", base, Duration::from_secs(5)).unwrap();
    let model = StabilityImageModel::from_parts(&client, "stable-audio-3", MediaOutput::Audio);

    let MediaOutcome::Deferred(job) = model.generate(&request()).await.unwrap() else {
        panic!("stable-audio-3 is deferred")
    };
    assert_eq!(job.0, id);
    let post = rx.recv().await.unwrap();
    assert_eq!(
        post.request_line(),
        "POST /v2beta/audio/stable-audio/text-to-audio HTTP/1.1"
    );
    assert!(post
        .body_text()
        .contains("name=\"model\"\r\n\r\nstable-audio-3"));

    assert_eq!(model.poll(&job).await.unwrap(), MediaPollOutcome::Pending);
    let pending = rx.recv().await.unwrap();
    assert_eq!(
        pending.request_line(),
        format!("GET /v2beta/audio/results/{id} HTTP/1.1")
    );
    assert_eq!(pending.header("accept").as_deref(), Some("audio/*"));

    let MediaPollOutcome::Complete(artifacts) = model.poll(&job).await.unwrap() else {
        panic!("the second poll completes")
    };
    assert_eq!(artifacts[0].mime, "audio/mpeg");
}

/// **The live construction path builds an audio model.** `MediaArm::from_slot` with an
/// `audio` slot must reach the Stable Audio route, not `stable-image/generate/*`; every
/// other test here constructs the model by hand, so this is the one that proves the
/// output is threaded through the real factory.
#[tokio::test]
async fn from_slot_on_an_audio_slot_dials_the_audio_route() {
    let (base, mut rx) = serve(vec![mp3_reply()]).await;
    let config = kaibo::config::Config::from_toml_str(&format!(
        r#"
        [backends.sd]
        kind = "stability"
        key_optional = true
        base_url = "{base}"

        [casts.foley]
        audio = "sd/stable-audio-2.5"
        "#
    ))
    .unwrap();
    let cast = config.resolve_cast("foley").unwrap();
    let slot = cast.slot(kaibo::config::ModelRole::Audio).unwrap();
    let backend = config.resolve_backend(&slot.backend).unwrap();
    let arm = kaibo::media::MediaArm::from_slot(backend, slot, MediaOutput::Audio).unwrap();

    arm.generate(&request()).await.expect("the fake answers");
    let cap = rx.recv().await.unwrap();
    assert_eq!(
        cap.request_line(),
        "POST /v2beta/audio/stable-audio-2/text-to-audio HTTP/1.1"
    );
}
