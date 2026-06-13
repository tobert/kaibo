//! Image generation — kaibo's first *capability* (vs. consultation).
//!
//! kaibo augments a calling agent with a team of models for two distinct kinds of
//! help: *consultation* (the Q&A tools, all `run_phase` costumes) and *capabilities*
//! — things the team can *do* and hand back as artifacts. This module is the first
//! capability: turn a prompt into image bytes.
//!
//! The seam is deliberately narrow. [`ImageGen`] is "prompt + size → bytes"; the
//! `generate_image` tool ([`crate::generate_image`]) owns everything around it
//! (cast/slot resolution, MIME, MCP delivery), and the offline tests drive a scripted
//! backend ([`ScriptedImageGen`]) so the whole tool is exercised with no network — the
//! same content-driven mock discipline as the completion loop ([`crate::test_support`]).
//!
//! **Why openai-only today.** rig 0.38 implements `ImageGenerationModel` for openai-kind
//! (and hf/xai), not for the keyed Anthropic/Gemini/DeepSeek protocols. kaibo's `openai`
//! kind is *any* OpenAI-compatible endpoint, so a hosted DALL·E/gpt-image or a local
//! Stable-Diffusion server speaking `/v1/images/generations` both work through the one
//! path. Non-openai kinds are refused at resolution (honest absence), the same call we
//! made for the parked TTS seam — not a silent no-op.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use rig_core::client::image_generation::ImageGenerationClient;
use rig_core::image_generation::ImageGenerationModel;
use rig_core::providers::openai;

use crate::config::{Backend, ModelSlot};
use crate::credentials::ProviderKind;

/// A model that turns a prompt into image bytes. One method, so the `generate_image`
/// tool is testable offline against [`ScriptedImageGen`] and the concrete provider is
/// swappable when rig grows more image backends.
#[async_trait]
pub trait ImageGen: Send + Sync {
    /// Generate one image for `prompt` at `size` (width, height). Returns the raw
    /// encoded bytes (PNG/JPEG/…); the caller sniffs the MIME and delivers them.
    async fn generate(&self, prompt: &str, size: (u32, u32)) -> Result<Vec<u8>>;
}

/// The rig-backed image generator over an OpenAI-compatible endpoint. Holds the rig
/// `ImageGenerationModel` built from the same base-URL/key/HTTP wiring an `openai` Arm
/// uses for completions ([`crate::consult::Arm::from_slot`]).
pub struct RigOpenaiImageGen {
    model: openai::ImageGenerationModel,
    /// Kept for error context only — which model/endpoint a failure came from.
    label: String,
}

impl RigOpenaiImageGen {
    /// Build an image generator from a resolved cast slot + its backend.
    ///
    /// Refuses a non-openai backend loudly: rig 0.38 has no image path for the keyed
    /// Anthropic/Gemini/DeepSeek protocols, so attaching one would only 400 (or worse,
    /// silently no-op) at call time. Honest absence beats a false promise.
    pub fn from_slot(backend: &Backend, slot: &ModelSlot) -> Result<Self> {
        if backend.kind != ProviderKind::Openai {
            return Err(anyhow!(
                "image generation is only available on openai-kind backends; backend {:?} \
                 is {:?} and rig 0.38 has no image support for it. Point the cast's `image` \
                 slot at an OpenAI-compatible endpoint (hosted gpt-image/DALL·E, or a local \
                 Stable-Diffusion server speaking /v1/images/generations).",
                backend.name,
                backend.kind
            ));
        }
        // Same wiring as the completion arm: a reqwest client carrying the backend's
        // per-request deadline, an OpenAI-compatible client on its base URL + key.
        let base_url = backend.resolved_base_url();
        let key = backend.resolve_key()?;
        let http = reqwest::Client::builder()
            .timeout(backend.request_timeout)
            .connect_timeout(
                backend
                    .request_timeout
                    .min(std::time::Duration::from_secs(10)),
            )
            .build()
            .map_err(|e| anyhow!("http client init: {e}"))?;
        // rig implements `ImageGenerationClient` for `openai::Client` (the Responses-ext
        // client), not `CompletionsClient` (the completion arm uses the latter). The
        // image model POSTs to `/v1/images/generations` regardless of the completion
        // ext, so the ext distinction is irrelevant here — only base_url/key matter.
        let client = openai::Client::builder()
            .api_key(&key)
            .base_url(&base_url)
            .http_client(http)
            .build()
            .map_err(|e| anyhow!("openai client init at {base_url}: {e}"))?;
        let model = client.image_generation_model(&slot.id);
        Ok(Self {
            model,
            label: format!("{} @ {}", slot.id, base_url),
        })
    }
}

#[async_trait]
impl ImageGen for RigOpenaiImageGen {
    async fn generate(&self, prompt: &str, size: (u32, u32)) -> Result<Vec<u8>> {
        let (width, height) = size;
        // The request is `#[non_exhaustive]`; build it through rig's typestate builder
        // (`prompt` flips Missing→Provided, which unlocks `.send()`).
        let resp = self
            .model
            .image_generation_request()
            .prompt(prompt)
            .width(width)
            .height(height)
            .send()
            .await
            .map_err(|e| anyhow!("image generation failed ({}): {e}", self.label))?;
        if resp.image.is_empty() {
            return Err(anyhow!(
                "image generation returned no bytes ({})",
                self.label
            ));
        }
        Ok(resp.image)
    }
}

/// Wrap any `Arc<dyn ImageGen>` builder result so the tool can hold one trait object.
pub fn rig_openai(backend: &Backend, slot: &ModelSlot) -> Result<Arc<dyn ImageGen>> {
    Ok(Arc::new(RigOpenaiImageGen::from_slot(backend, slot)?))
}

#[cfg(test)]
mod test_double {
    use super::*;
    use std::sync::Mutex;

    /// A scripted [`ImageGen`] for offline tests: returns fixed bytes and records the
    /// `(prompt, size)` it was called with, so a test can assert what reached the
    /// backend without a network or a real model — the image-gen analogue of
    /// [`crate::test_support::ScriptedClient`].
    pub struct ScriptedImageGen {
        bytes: Vec<u8>,
        calls: Mutex<Vec<(String, (u32, u32))>>,
        fail_with: Option<String>,
    }

    impl ScriptedImageGen {
        /// Returns `bytes` for every call.
        pub fn returning(bytes: Vec<u8>) -> Self {
            Self {
                bytes,
                calls: Mutex::new(Vec::new()),
                fail_with: None,
            }
        }

        /// Fails every call with `msg` — drives the tool's error path.
        pub fn failing(msg: &str) -> Self {
            Self {
                bytes: Vec::new(),
                calls: Mutex::new(Vec::new()),
                fail_with: Some(msg.to_string()),
            }
        }

        /// The `(prompt, size)` of each call, in order.
        pub fn calls(&self) -> Vec<(String, (u32, u32))> {
            self.calls.lock().expect("calls lock").clone()
        }
    }

    #[async_trait]
    impl ImageGen for ScriptedImageGen {
        async fn generate(&self, prompt: &str, size: (u32, u32)) -> Result<Vec<u8>> {
            self.calls
                .lock()
                .expect("calls lock")
                .push((prompt.to_string(), size));
            if let Some(msg) = &self.fail_with {
                return Err(anyhow!("{msg}"));
            }
            Ok(self.bytes.clone())
        }
    }
}

#[cfg(test)]
pub use test_double::ScriptedImageGen;

#[cfg(test)]
mod resolver_tests {
    use super::*;
    use crate::config::ModelSlot;
    use std::time::Duration;

    fn backend(kind: ProviderKind) -> Backend {
        Backend {
            name: format!("{kind:?}").to_lowercase(),
            kind,
            base_url: Some("http://localhost:13305/api/v1".into()),
            api_key_env: None,
            api_key_file: None,
            key_optional: true,
            request_timeout: Duration::from_secs(30),
        }
    }

    /// An openai-kind image slot builds a generator (offline — only the client is
    /// constructed; no `/v1/images/generations` call until `generate`).
    #[test]
    fn openai_slot_builds() {
        let b = backend(ProviderKind::Openai);
        let slot = ModelSlot::bare("local", "sdxl-turbo");
        assert!(RigOpenaiImageGen::from_slot(&b, &slot).is_ok());
    }

    /// A non-openai backend is refused loudly — rig 0.38 has no image path for the
    /// keyed protocols, so attaching one would only fail (or worse, no-op) at call
    /// time. Honest absence, the same call as the parked TTS seam.
    #[test]
    fn non_openai_slot_is_refused() {
        for kind in [
            ProviderKind::Anthropic,
            ProviderKind::Gemini,
            ProviderKind::DeepSeek,
        ] {
            let b = backend(kind);
            let slot = ModelSlot::bare(b.name.clone(), "some-image-model");
            // `RigOpenaiImageGen` isn't `Debug` (the rig model isn't), so match
            // rather than `expect_err`.
            let err = match RigOpenaiImageGen::from_slot(&b, &slot) {
                Ok(_) => panic!("non-openai image backend ({kind:?}) must be refused"),
                Err(e) => e,
            };
            assert!(
                err.to_string().contains("only available on openai-kind"),
                "error should explain the openai-only constraint: {err}"
            );
        }
    }
}
