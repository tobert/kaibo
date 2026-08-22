//! The media-generation seam: kaibo's counterpart of rig's `CompletionModel` for
//! backends that produce artifacts instead of text.
//!
//! # Why a kaibo trait, when rig ships `ImageGenerationModel`
//!
//! rig's trait is one synchronous text-to-image call (`prompt`/`width`/`height`), with
//! nowhere to hand a deferred job id — and Stability's v2beta family is bigger than
//! that (deferred upscale/audio/3D operations whose POST returns only an id; see
//! `src/stability.rs`'s module doc). kaibo's tool lane needs the deferred half as a
//! first-class outcome: a sync operation answers in-call, a deferred one becomes a
//! kaibo job the caller polls. So [`MediaModel`] carries both, the way
//! [`crate::batch::BatchProvider`] carries submit *and* poll.
//!
//! The vocabulary here is deliberately provider-neutral (a [`MediaArtifact`] is bytes,
//! a mime string, and a seed) — providers translate their native shapes at their own
//! impl, exactly as rig providers translate into rig's completion types. Two live
//! implementations today: Stability's (`src/stability.rs`, multipart form wire, sync
//! and deferred shapes) and OpenAI Images (`src/openai_images.rs`, JSON body wire,
//! sync only).
//!
//! # The construction point
//!
//! [`MediaArm::from_slot`] is the single place a media backend becomes callable — the
//! media mirror of `Arm::from_slot` (`consult/engine.rs`), and the enforcement seam
//! for the completion/media split: it accepts only [`ProviderClass::Media`] kinds and
//! bails loudly on a completion backend, the mirror image of `Arm::from_slot`'s
//! media bail. Config validation (`config.rs`) refuses the mismatched pairings at
//! load, so both bails are belts to those braces.
//!
//! Nothing here touches the filesystem: a [`MediaArtifact`]'s bytes live in memory
//! until a caller hands them to the media CAS (`src/cas.rs`), which is one of the two
//! blessed write surfaces and does its own containment checks.

use std::sync::Arc;

use anyhow::{bail, Result};
use async_trait::async_trait;

use crate::config::{Backend, ModelSlot};
use crate::credentials::{MediaKind, ProviderClass};

/// One provider-native field value, carrying the caller's JSON type end to end. The
/// caller said string, number, or bool at the tool face; guessing the type back out
/// of a string is unsound in both directions (the Images API's `user` field is a
/// string — `user = "123"` re-typed to the number `123` is a provider 400), so the
/// stated type rides through instead. Each provider serializes it for its own wire:
/// a JSON-body provider (openai-images) sends it verbatim as the JSON scalar it is;
/// a form-field provider (Stability) stringifies via
/// [`to_wire_string`](Self::to_wire_string), which is lossless there because its
/// multipart wire is all-string anyway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldValue {
    Str(String),
    /// `serde_json::Number`, not `f64`: it keeps integers integral (`n = 2` must
    /// serialize as `2`, never `2.0` — the Images API type-checks it) and carries
    /// the full u64/i64/f64 range a caller can write in JSON.
    Num(serde_json::Number),
    Bool(bool),
}

impl FieldValue {
    /// The value as the JSON scalar the caller stated — what a JSON-body provider
    /// sends verbatim.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            FieldValue::Str(s) => serde_json::Value::String(s.clone()),
            FieldValue::Num(n) => serde_json::Value::Number(n.clone()),
            FieldValue::Bool(b) => serde_json::Value::Bool(*b),
        }
    }

    /// The value as wire text — what a form-field provider sends. Numbers and bools
    /// render in their JSON spelling (`2`, `1.5`, `true`), so nothing is lost on an
    /// all-string wire.
    pub fn to_wire_string(&self) -> String {
        match self {
            FieldValue::Str(s) => s.clone(),
            FieldValue::Num(n) => n.to_string(),
            FieldValue::Bool(b) => b.to_string(),
        }
    }

    /// The string inside a `Str`, or `None` — for the fields a provider itself must
    /// read (an `output_format` name), where a number or bool is the caller's
    /// mistake to surface, not a value to coerce.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            FieldValue::Str(s) => Some(s),
            _ => None,
        }
    }
}

impl From<&str> for FieldValue {
    fn from(s: &str) -> Self {
        FieldValue::Str(s.to_string())
    }
}

impl From<String> for FieldValue {
    fn from(s: String) -> Self {
        FieldValue::Str(s)
    }
}

/// One generation request, provider-neutral. The prompt is the portable half; every
/// provider-native knob (seed, aspect ratio, negative prompt, output format, ...)
/// rides `fields` with no allowlist — kaibo passes them through, the provider answers
/// for its own knobs, the same passthrough posture `additional_params` takes on the
/// completion side. (`prompt` and `model` never ride `fields`: the tool layer
/// reserves both, so the recorded provenance always describes the request that ran.)
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MediaRequest {
    pub prompt: String,
    /// `(name, value)` provider-native fields: **uniquely named**, in the order the
    /// caller gave them (kept as a `Vec` because a provider may care about order —
    /// the MCP face is a map, so duplicate names cannot arrive from a tool call).
    /// Values are typed ([`FieldValue`]) so the caller's JSON type survives to the
    /// provider's wire. A provider impl may seed its own defaults (Stability seeds
    /// `output_format`) and lets a caller field of the same name replace the seeded
    /// default — that merge is the provider's business, not a contract of this
    /// struct.
    pub fields: Vec<(String, FieldValue)>,
    /// The binary parts this operation carries, in the order the caller gave them.
    /// Empty for text-to-image.
    ///
    /// **Named, and plural, because the provider's operations are.** A first cut had a
    /// single `input_image: Option<Vec<u8>>`, which fits `edit/outpaint` and
    /// `control/sketch` and fits nothing else: `edit/erase` and `edit/inpaint` take
    /// `image` *and* `mask`, `control/style-transfer` takes `init_image` *and*
    /// `style_image`, and `edit/replace-background-and-relight` takes three. The field
    /// *name* varies too — `image`, `audio`, `subject_image` — so the name is data, not
    /// a constant a provider impl can assume. This is the text-part `fields` list's
    /// binary sibling, and it is a `Vec` for the same reason: a provider may care about
    /// order.
    pub inputs: Vec<MediaInput>,
}

/// One binary part of a generation request: which form field it fills, what to call it
/// on the wire, and the bytes.
///
/// The filename is carried rather than derived. A multipart file part without a
/// `filename=` is rejected or mis-typed by some servers, and guessing one from the bytes
/// would put a second, weaker format opinion next to the one the media store already
/// holds — the store is the authority on what an object is (`MediaStore::extension_for`),
/// so the caller that resolved the bytes passes the name the store gave them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaInput {
    /// The provider's own form-field name — `image`, `mask`, `init_image`, `audio`.
    pub field: String,
    /// The filename to send with the part, extension included (`image.png`).
    pub filename: String,
    pub bytes: Vec<u8>,
}

impl MediaInput {
    /// One input part, with the filename built from the field name and the extension the
    /// store reported for these bytes.
    pub fn new(field: impl Into<String>, extension: &str, bytes: Vec<u8>) -> Self {
        let field = field.into();
        Self {
            filename: format!("{field}.{extension}"),
            field,
            bytes,
        }
    }
}

/// Resolve caller-named `{form-field: digest}` pairs into request parts, reading the
/// bytes out of the media store.
///
/// This is the join that makes the media lane compose: `write_cas` (or a previous
/// `generate`) hands the caller a digest, and the caller feeds that digest straight back
/// in as the input to an edit. Digests are the operator's currency for exactly this — no
/// bytes cross the wire twice, and the chain from source image to edited result is
/// recorded in the store at every hop.
///
/// The store, not the caller, says what each object is: the filename's extension comes
/// from [`crate::cas::MediaStore::extension_for`], so a part can never be labelled
/// something the object is not.
///
/// Every failure is named and nothing is sent: a bad digest, a digest for an object this
/// store does not hold, or an object that is not an image all refuse before a request is
/// built.
pub fn resolve_inputs(
    store: &crate::cas::MediaStore,
    asked: &std::collections::BTreeMap<String, String>,
) -> Result<Vec<MediaInput>> {
    let mut out = Vec::with_capacity(asked.len());
    for (field, digest_hex) in asked {
        let digest = crate::cas::Digest::from_hex(digest_hex).map_err(|_| {
            anyhow::anyhow!(
                "`inputs.{field}` is not a digest: {digest_hex:?}. A digest is the 64 \
                 lowercase hex characters `write_cas` and `generate` hand back — the tail \
                 of a kaibo://cas/<digest> address."
            )
        })?;
        let (bytes, extension) = store
            .get(&digest)
            .map_err(|e| {
                anyhow::anyhow!("`inputs.{field}`: reading {digest_hex} from the media store: {e}")
            })?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "`inputs.{field}`: this store holds no object at {digest_hex}. Store \
                     the image with `write_cas` first and pass the digest it returns."
                )
            })?;
        if !extension.is_image() {
            bail!(
                "`inputs.{field}`: the object at {digest_hex} is {}, not an image. An \
                 input part is an image; pass the digest of one.",
                extension.mime()
            );
        }
        out.push(MediaInput::new(field.clone(), extension.as_str(), bytes));
    }
    Ok(out)
}

/// Refuse a request carrying binary inputs on a provider that has no route for them.
///
/// **Dropping them is the failure this exists to prevent.** A provider impl that ignored
/// `inputs` would send the prompt alone, get back a plausible image, and store it with a
/// digest and a provenance sidecar — the caller asked to edit *this* picture and received
/// an unrelated one that looks like a success. That is silent corruption of the result,
/// and it is far worse than a refusal naming the two ways forward.
pub fn refuse_binary_inputs(request: &MediaRequest, provider: &str) -> Result<()> {
    if request.inputs.is_empty() {
        return Ok(());
    }
    let named: Vec<&str> = request.inputs.iter().map(|i| i.field.as_str()).collect();
    bail!(
        "this call carries the input image{} `{}`, and the {provider} backend has no \
         operation that accepts one — nothing was generated. Point the cast's `image` \
         slot at a backend whose operations take an input image (Stability), or drop \
         `inputs` to generate from the prompt alone.",
        if named.len() == 1 { "" } else { "s" },
        named.join("`, `"),
    )
}

/// One generated artifact, provider-neutral: the bytes, the wire content type in the
/// provider's own spelling (`"image/png"`), and the provider-reported seed when one
/// exists — the reproduction handle the CAS provenance sidecar records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaArtifact {
    pub bytes: Vec<u8>,
    pub mime: String,
    pub seed: Option<String>,
}

/// The id of one deferred generation, as the provider spelled it. Opaque: it
/// round-trips through kaibo's job store as a string and only the provider's own
/// poll route interprets it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaJobId(pub String);

/// One `generate` call's outcome. Sync operations resolve straight to `Complete`;
/// a deferred operation's POST resolves to `Deferred`, and the artifacts come from
/// [`MediaModel::poll`] later. Which shape a given operation has is the *provider's*
/// declared fact (see `stability::Operation::shape`), never sniffed from a response.
///
/// `Complete` carries a **list**: many image models return several images per call
/// (Amy's call, 2026-08-03), so one generation is one-to-many artifacts. Each artifact
/// gets its own CAS digest and its own provenance sidecar downstream — a result is a
/// list of digests, never a single blessed one. A provider that returns exactly one
/// (Stability's ops today) hands back a one-element list; an *empty* list from a
/// provider is that provider's bug to refuse at its own impl, not a shape this enum
/// blesses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaOutcome {
    Complete(Vec<MediaArtifact>),
    Deferred(MediaJobId),
}

/// One poll's outcome: still running, or done. A poll is never itself deferred
/// again — `Pending` means "ask again later", with the cadence owned by the caller
/// (the tool lane brings its own deadline/backoff; this seam never sleeps).
/// `Complete` is a list for the same one-to-many reason as
/// [`MediaOutcome::Complete`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaPollOutcome {
    Pending,
    Complete(Vec<MediaArtifact>),
}

/// A media-generation model: one generate call plus one poll, both provider-neutral.
/// Object-safe (`Arc<dyn MediaModel>`) so the tool lane and tests dispatch over the
/// same seam — a scripted impl drives the lane offline, mirroring how
/// [`crate::batch::BatchProvider`] is exercised against `ScriptedBatch`.
#[async_trait]
pub trait MediaModel: Send + Sync {
    /// Run one generation request against the model this instance was built for.
    async fn generate(&self, request: &MediaRequest) -> Result<MediaOutcome>;

    /// Collect one deferred job's result. One shot, no retry loop — the caller owns
    /// the cadence.
    async fn poll(&self, job: &MediaJobId) -> Result<MediaPollOutcome>;
}

/// A staffed media slot, ready to call: the model behind an `image = "backend/id"`
/// cast slot. The media mirror of the completion `Arm`.
#[derive(Clone)]
pub struct MediaArm {
    model: Arc<dyn MediaModel>,
    /// The slot's `"backend/model-id"` ref — provenance and error text.
    slot_ref: String,
}

impl std::fmt::Debug for MediaArm {
    /// Manual: `dyn MediaModel` carries no `Debug` bound, and the slot ref is the
    /// arm's whole identity anyway.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaArm")
            .field("slot_ref", &self.slot_ref)
            .finish_non_exhaustive()
    }
}

impl MediaArm {
    /// Wrap an already-built model — the seam tests inject a scripted [`MediaModel`]
    /// through, mirroring `Arm::new` on the completion side. Production goes through
    /// [`from_slot`](Self::from_slot).
    pub fn new(model: Arc<dyn MediaModel>, slot_ref: impl Into<String>) -> Self {
        Self {
            model,
            slot_ref: slot_ref.into(),
        }
    }

    /// The single live construction point: resolve a media cast slot into a callable
    /// arm. Only a [`ProviderClass::Media`] backend can staff one — a completion
    /// backend is refused here with the same load-guard framing `Arm::from_slot` uses
    /// for the mirror-image mistake.
    pub fn from_slot(backend: &Backend, slot: &ModelSlot) -> Result<Self> {
        match backend.kind.class() {
            ProviderClass::Media(MediaKind::Stability) => {
                let key = backend.resolve_key()?;
                let base_url = backend
                    .base_url
                    .clone()
                    .unwrap_or_else(|| crate::stability::STABILITY_API_BASE.to_string());
                let client =
                    crate::stability::StabilityClient::new(key, base_url, backend.request_timeout)?;
                let model = crate::stability::StabilityImageModel::from_parts(&client, &slot.id);
                Ok(Self::new(Arc::new(model), slot.qualified()))
            }
            ProviderClass::Media(MediaKind::OpenAiImages) => {
                // Key sources are shared with the `openai` completion kind, but the
                // key is required by default (the default endpoint is hosted OpenAI
                // — see `ProviderKind::key_optional`); a keyless local sd-server
                // backend sets `key_optional = true` and resolves the placeholder.
                let key = backend.resolve_key()?;
                // Unset dials hosted OpenAI — a root through /v1; the client appends
                // its own /images/generations. A local sd-server sets base_url.
                let base_url = backend
                    .base_url
                    .clone()
                    .unwrap_or_else(|| crate::credentials::HOSTED_OPENAI_BASE_URL.to_string());
                let client = crate::openai_images::OpenAiImagesClient::new(
                    key,
                    base_url,
                    backend.request_timeout,
                )?;
                let model = crate::openai_images::OpenAiImagesModel::from_parts(&client, &slot.id);
                Ok(Self::new(Arc::new(model), slot.qualified()))
            }
            ProviderClass::Media(MediaKind::DashScope) => {
                // Keyed with no keyless target, so a missing credential is a hard
                // error here rather than a placeholder bearer.
                let key = backend.resolve_key()?;
                // Unset dials DashScope's shared international root; a dedicated
                // endpoint sets its own. Either way the client appends the route.
                let base_url = backend
                    .base_url
                    .clone()
                    .unwrap_or_else(|| crate::dashscope::DASHSCOPE_API_BASE.to_string());
                let client =
                    crate::dashscope::DashScopeClient::new(key, base_url, backend.request_timeout)?;
                let model = crate::dashscope::DashScopeImageModel::from_parts(&client, &slot.id);
                Ok(Self::new(Arc::new(model), slot.qualified()))
            }
            ProviderClass::Wire(_) => bail!(
                "backend {:?} is kind `{}`, a completion wire — it cannot staff a media \
                 slot. Point the `image` slot at a media backend, and use this backend \
                 on `explorer`/`synth` instead",
                backend.name,
                backend.kind.canonical_name()
            ),
        }
    }

    /// The `"backend/model-id"` this arm was staffed from.
    pub fn slot_ref(&self) -> &str {
        &self.slot_ref
    }

    /// Run one generation request on this arm.
    pub async fn generate(&self, request: &MediaRequest) -> Result<MediaOutcome> {
        self.model.generate(request).await
    }

    /// Collect one deferred job's result.
    pub async fn poll(&self, job: &MediaJobId) -> Result<MediaPollOutcome> {
        self.model.poll(job).await
    }
}

/// How the generate lane turns a cast's `image` slot into a callable [`MediaArm`] —
/// the injection seam, mirroring [`crate::batch::BatchProviderFactory`]: the handler
/// holds an `Arc<dyn MediaArmFactory>`, production seeds [`LiveMediaArms`] (the real
/// [`MediaArm::from_slot`] path), and tests swap in a factory returning a scripted
/// [`MediaModel`] so the whole tool lane — CAS writes, job lane, rendering — runs
/// offline with no network.
pub trait MediaArmFactory: Send + Sync {
    fn build(&self, backend: &Backend, slot: &ModelSlot) -> Result<MediaArm>;
}

/// The real construction path: [`MediaArm::from_slot`], the single live point where a
/// media backend becomes callable.
pub struct LiveMediaArms;

impl MediaArmFactory for LiveMediaArms {
    fn build(&self, backend: &Backend, slot: &ModelSlot) -> Result<MediaArm> {
        MediaArm::from_slot(backend, slot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scripted model: answers `generate` with a fixed outcome and `poll` with a
    /// pending-then-complete sequence — enough to prove the trait dispatches as an
    /// object and the arm forwards faithfully, artifact *lists* included.
    struct Scripted {
        artifacts: Vec<MediaArtifact>,
        polls: std::sync::Mutex<usize>,
    }

    #[async_trait]
    impl MediaModel for Scripted {
        async fn generate(&self, request: &MediaRequest) -> Result<MediaOutcome> {
            if request.prompt.is_empty() {
                bail!("empty prompt");
            }
            Ok(MediaOutcome::Deferred(MediaJobId("job-1".to_string())))
        }

        async fn poll(&self, job: &MediaJobId) -> Result<MediaPollOutcome> {
            assert_eq!(job.0, "job-1");
            let mut n = self.polls.lock().unwrap();
            *n += 1;
            if *n == 1 {
                Ok(MediaPollOutcome::Pending)
            } else {
                Ok(MediaPollOutcome::Complete(self.artifacts.clone()))
            }
        }
    }

    fn png_artifact() -> MediaArtifact {
        MediaArtifact {
            bytes: vec![0x89, b'P', b'N', b'G'],
            mime: "image/png".to_string(),
            seed: Some("42".to_string()),
        }
    }

    fn webp_artifact() -> MediaArtifact {
        MediaArtifact {
            bytes: b"RIFF....WEBP".to_vec(),
            mime: "image/webp".to_string(),
            seed: None,
        }
    }

    /// The deferred round trip through the trait object: generate hands back a job,
    /// the first poll is pending, the second completes with EVERY artifact intact and
    /// in order — the one-to-many shape (many image models return several images per
    /// call) the outcome enums exist to carry.
    #[tokio::test]
    async fn deferred_generate_then_poll_round_trip_with_multiple_artifacts() {
        let arm = MediaArm::new(
            Arc::new(Scripted {
                artifacts: vec![png_artifact(), webp_artifact()],
                polls: std::sync::Mutex::new(0),
            }),
            "sd/core",
        );
        assert_eq!(arm.slot_ref(), "sd/core");
        let outcome = arm
            .generate(&MediaRequest {
                prompt: "a lighthouse".to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        let MediaOutcome::Deferred(job) = outcome else {
            panic!("scripted generate is deferred, got {outcome:?}");
        };
        assert_eq!(arm.poll(&job).await.unwrap(), MediaPollOutcome::Pending);
        let done = arm.poll(&job).await.unwrap();
        assert_eq!(
            done,
            MediaPollOutcome::Complete(vec![png_artifact(), webp_artifact()])
        );
    }

    /// A completion backend cannot staff a media arm: the mirror image of
    /// `Arm::from_slot`'s media bail, and the message names both the mistake and
    /// the fix.
    #[test]
    fn from_slot_refuses_a_completion_backend() {
        let cfg = crate::config::Config::builtin();
        let backend = cfg.backends.get("anthropic").expect("built-in exists");
        let slot = ModelSlot::bare("anthropic", "claude-sonnet-4-6");
        let err = MediaArm::from_slot(backend, &slot).expect_err("wire kind must be refused");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("completion wire") && msg.contains("media backend"),
            "the error names the problem and the fix, got: {msg}"
        );
    }

    /// An openai-images backend staffs an image slot. The local sd-server shape —
    /// explicit `base_url` plus explicit `key_optional = true` (the kind seeds
    /// key-REQUIRED, because its default endpoint is hosted OpenAI) — resolves the
    /// placeholder and needs no credential. Construction only; no network.
    #[test]
    fn from_slot_staffs_an_openai_images_backend_keylessly() {
        let cfg = crate::config::Config::from_toml_str(
            r#"
            [backends.sdcpp]
            kind = "openai-images"
            base_url = "http://localhost:1234/v1"
            key_optional = true
            api_key_file = "/nonexistent-kaibo-test/openai"
            "#,
        )
        .expect("config parses");
        let backend = cfg.backends.get("sdcpp").expect("backend exists");
        let slot = ModelSlot::bare("sdcpp", "sd3.5-large");
        let arm = MediaArm::from_slot(backend, &slot).expect("an openai-images backend staffs");
        assert_eq!(arm.slot_ref(), "sdcpp/sd3.5-large");
    }

    /// A DashScope backend staffs an image slot. Keyed, so the key must resolve —
    /// this fixture supplies one through `api_key_file`'s env-var expansion being
    /// absent and `key_optional` left false, which means the *file* must exist; so
    /// the test uses the env source instead via a literal key on the backend's
    /// configured env var. Construction only; no network.
    #[test]
    fn from_slot_staffs_a_dashscope_backend() {
        let cfg = crate::config::Config::from_toml_str(
            r#"
            [backends.wan]
            kind = "dashscope"
            base_url = "https://dashscope-intl.example/"
            key_optional = true
            api_key_file = "/nonexistent-kaibo-test/dashscope"
            "#,
        )
        .expect("config parses");
        let backend = cfg.backends.get("wan").expect("backend exists");
        let slot = ModelSlot::bare("wan", "wan2.6-t2i");
        let arm = MediaArm::from_slot(backend, &slot).expect("a dashscope backend staffs");
        assert_eq!(arm.slot_ref(), "wan/wan2.6-t2i");
    }

    /// The Stability arm still staffs — the sibling-kind regression guard for the
    /// `from_slot` match growing a second media arm.
    #[test]
    fn from_slot_still_staffs_a_stability_backend() {
        let cfg = crate::config::Config::from_toml_str(
            r#"
            [backends.sd]
            kind = "stability"
            key_optional = true
            "#,
        )
        .expect("config parses");
        let backend = cfg.backends.get("sd").expect("backend exists");
        let slot = ModelSlot::bare("sd", "core");
        let arm = MediaArm::from_slot(backend, &slot).expect("a stability backend staffs");
        assert_eq!(arm.slot_ref(), "sd/core");
    }
}
