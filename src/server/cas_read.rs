//! `read_cas` — the client's retrieval half of the artifact contract.
//!
//! kaibo's producers (`generate`, `save_artifact`) hand back a digest and never inline
//! bytes. This is where those bytes come back, and it is a **tool** rather than the
//! `kaibo://cas/<digest>` MCP *resource* it replaces. Both halves of that swap were
//! deliberate.
//!
//! # Why a tool and not a resource
//!
//! **Resources are ambient; a tool call is deliberate.** MCP hosts treat resources as
//! attachable context — some prefetch them, some auto-attach them to a turn — which is a
//! reasonable posture for a config file kaibo publishes and the wrong one for
//! model-authored content that a producer just minted. A tool call is explicit: named
//! arguments the caller chose, a permission prompt in most hosts, and a traced call.
//! Retrieval of bytes a model wrote should be something the operator's agent *does*, not
//! something that happens to its context.
//!
//! **`resources/read` has no negotiation.** It is whole-blob by construction: no range,
//! no size hint, no way to ask what an object is before pulling it. Measured on a real
//! 3.8 MB PNG, one read produced roughly 5 MB of base64. Claude Code spilled it to a side
//! file — host mercy, not protocol — and a host without that reflex would have inlined
//! the lot. A tool can answer "what is this?" for a few dozen tokens, hand back a bounded
//! window by default, and let the caller page deliberately.
//!
//! The URI **string** survives: `kaibo://cas/<digest>` is still how an artifact is named
//! in a footer, an answer, or a `generate` result. It is an identifier, and `read_cas`
//! takes the digest out of it. Only the `resources/read` serving of it is gone.
//!
//! # The contract
//!
//! - **Metadata always leads**, on every response: digest, mime, total bytes, whether the
//!   object is binary, the label its sidecar carries, the range actually served, and — in
//!   disk mode — the real filesystem path, so a caller holding a shell can go direct
//!   instead of paging bytes through a model's context.
//! - **Bounded by default.** No `length` gives a [`DEFAULT_READ_BYTES`] window from
//!   `offset`, never the whole object; the metadata names the total so the caller decides
//!   whether to page. An explicit `length` may ask for more, up to [`MAX_READ_BYTES`].
//! - **Never a default base64 dump.** A body is served without being asked for only when
//!   the object is textual. Binary content reaches a caller two ways and no others: the
//!   one-hop image path below, or an explicit `length`.
//! - **Images get one hop to the eye.** A small enough image with no range asked for
//!   comes back as an MCP image content block, which hosts render straight to a vision
//!   model — the same mechanism kaibo's inner `view_image` rides. A base64 slice of a PNG
//!   helps nobody, so it is never the default.
//!
//! # Verification is not negotiable, so a read is whole-object
//!
//! Ranges are computed *after* [`crate::cas::Cas::get`] has read and verified the whole
//! object. That is deliberate: the store's guarantee is that content is checked against
//! its digest before a single byte reaches a caller (see `cas.rs`'s module doc on why a
//! streaming read could not inherit it), and serving a range straight off disk would
//! quietly trade that away for local I/O that costs nothing in the only budget that
//! matters here — the caller's context. "Cheap" in this module means cheap in tokens.
//! A metadata-only read verifies too, so a corrupt object is loud on every path.

use std::path::Path;

use crate::cas::Extension;

/// The window a read returns when the caller names no `length`: enough to see what an
/// object holds, far short of the whole thing. The metadata reports the total, so paging
/// is a deliberate second call rather than something that happens to a context window.
pub const DEFAULT_READ_BYTES: usize = 1 << 16;

/// The ceiling on an explicit `length`. Deliberately the same figure as
/// `artifact::MAX_ARTIFACT_BYTES`: the largest thing a model may write in one artifact is
/// the largest thing a client may pull in one read, which makes the two halves of the
/// contract one number to remember. A megabyte of text is already ~260K tokens, so this
/// is a real ceiling rather than a formality — past it the caller pages.
pub const MAX_READ_BYTES: usize = 1 << 20;

/// The largest image `read_cas` will hand back as a rendered image content block.
///
/// Sized loose on purpose (Amy, 2026-08-05: "5MB and we'll see who complains"): a
/// 1024×1024 PNG from Stability or gpt-image lands around 1–2 MB, so the common case of
/// "look at what I just generated" takes one hop with room over it. Above the line the
/// caller gets metadata and (in disk mode) a path, because base64 inflates by about a
/// third on the way into a context window and the useful move for a very large image is
/// to open the file, not to inline it. If hosts complain (Anthropic's per-image API cap
/// is ~5 MB and base64 of 5 MiB lands past it), tighten here — one constant.
pub const INLINE_IMAGE_MAX_BYTES: usize = 5 << 20;

/// One stored object, resolved and verified, as [`plan`] sees it.
pub struct CasObject<'a> {
    pub digest: &'a str,
    pub ext: Extension,
    pub bytes: &'a [u8],
    /// The model's own one-line description, when the sidecar carries one.
    ///
    /// `None` covers two different situations, which is why it does not stand alone: a
    /// record that carries no label, and no record at all. See `provenance_present`.
    pub label: Option<&'a str>,
    /// Whether this object has a provenance record beside it.
    ///
    /// Separate from `label` because "the record says nothing about this" and "there is
    /// no record" are different facts, and only the second is worth a line. An object can
    /// legitimately lack one: `CasError::ProvenanceNotRecorded` mints exactly this state
    /// (the object landed, the sidecar write failed), and the extension-probe fallback in
    /// `Cas::entry_for` serves objects whose sidecar is missing or unreadable. This store
    /// never rewrites, so such an object stays recordless forever — worth saying once,
    /// rather than leaving a caller to read "no label" as "nothing was written down".
    ///
    /// The sidecar is best-effort housekeeping, not a promised record (Amy's ruling), so
    /// a *present* record with no label renders nothing at all: a `label: (none)` line on
    /// every generated image would be noise on the common case.
    pub provenance_present: bool,
    /// The real filesystem path — disk mode only. Memory mode has no file.
    pub path: Option<&'a Path>,
}

/// What a read hands back beside its metadata.
#[derive(Debug, PartialEq, Eq)]
pub enum Body {
    /// Metadata only: an explicit `length: 0`, an offset past the end, or a binary
    /// object nobody asked for a range of.
    None,
    /// A textual slice, decoded.
    Text(String),
    /// A binary slice, base64 of exactly the bytes named in the metadata.
    Base64(String),
    /// The whole image, as a content block a host renders.
    Image { data: String, mime: &'static str },
}

/// A planned response: the metadata block that always leads, and the body.
#[derive(Debug, PartialEq, Eq)]
pub struct CasView {
    pub meta: String,
    pub body: Body,
}

/// Plan one read. Pure over an already-verified object, so every rule here is testable
/// without a store, a handler, or a network.
///
/// `Err` is the one refusal that belongs to this layer: a `length` past
/// [`MAX_READ_BYTES`]. Refused rather than clamped, because a caller that asked for 4 MiB
/// and silently got 1 MiB would page from the wrong place next time; the message names
/// the ceiling and the recovery.
/// How the caller's side will deliver a body, which changes exactly one sentence: what a
/// whole small image is *served as*.
///
/// The rules of a read belong to one planner, but the claim "as a rendered image" is only
/// true where a host turns an image block into pixels. A terminal gets bytes on a
/// descriptor, and saying otherwise would be kaibo describing something that did not
/// happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// An MCP host renders the image content block.
    Rendered,
    /// The bytes go to a stream the caller already aimed (the CLI).
    Bytes,
}

pub fn plan(
    obj: &CasObject,
    offset: usize,
    length: Option<usize>,
    delivery: Delivery,
) -> Result<CasView, String> {
    if let Some(asked) = length {
        if asked > MAX_READ_BYTES {
            return Err(format!(
                "`length` {asked} is past the {MAX_READ_BYTES}-byte ceiling on a single \
                 read. Ask for at most {MAX_READ_BYTES} bytes and walk the object with \
                 `offset` — every response's metadata names the total size, so you can \
                 page without guessing."
            ));
        }
    }
    let total = obj.bytes.len();
    let start = offset.min(total);

    // The one-hop visual path, and the only case that serves a whole object: an image,
    // small enough to be worth a context window, that nobody asked for a slice of.
    if obj.ext.is_image()
        && offset == 0
        && length.is_none()
        && total > 0
        && total <= INLINE_IMAGE_MAX_BYTES
    {
        return Ok(CasView {
            meta: render_meta(
                obj,
                total,
                &match delivery {
                    Delivery::Rendered => {
                        format!("the whole object, {total} bytes, as a rendered image")
                    }
                    Delivery::Bytes => format!("the whole object, {total} bytes"),
                },
                None,
            ),
            body: Body::Image {
                data: encode(obj.bytes),
                mime: obj.ext.mime(),
            },
        });
    }

    // How much to serve when the caller named no length. Textual content gets a window
    // because a window of text is *readable*; binary gets nothing, because 64 KiB of
    // base64 PNG is not a preview of anything. Binary reaches a caller only through an
    // explicit `length` or the image path above.
    let want = match length {
        Some(asked) => asked,
        None if obj.ext.is_textual() => DEFAULT_READ_BYTES,
        None => 0,
    };
    let end = start.saturating_add(want).min(total);

    if end <= start {
        let why = if total == 0 {
            "nothing: this object is empty".to_string()
        } else if start >= total {
            format!("nothing: offset {offset} is at or past the end of {total} bytes")
        } else if length.is_some() {
            "nothing: metadata only".to_string()
        } else if obj.path.is_some() {
            "nothing: this object is binary, so pass a `length` for a base64 range, or \
             open the file at the path above"
                .to_string()
        } else {
            "nothing: this object is binary, so pass a `length` for a base64 range".to_string()
        };
        return Ok(CasView {
            meta: render_meta(obj, total, &why, None),
            body: Body::None,
        });
    }

    // Textual content comes back as the string it is. A window can land mid-character, so
    // serve the largest whole-character range inside it and report THAT range — the
    // caller's next offset has to be a byte position it can actually resume from.
    if obj.ext.is_textual() {
        match text_window(obj.bytes, start, end) {
            TextWindow::Whole { to, text } => {
                return Ok(CasView {
                    meta: render_meta(obj, total, &format!("bytes {start}..{to} of {total}"), None),
                    body: Body::Text(text.to_string()),
                })
            }
            // **The cursor must always move.** A window too narrow to hold one whole
            // character used to trim to nothing and report `bytes N..N`; a caller
            // resuming at the endpoint it was handed read the same byte forever, with no
            // way to detect it. Serving the exact bytes as base64 is imperfect and
            // *escapable*: the range advances over them, the note says why they are not
            // text, and the caller widens `length` or realigns `offset`.
            //
            // Refusing was the other option and is worse. A mechanical pager cannot act
            // on a refusal; always-advances is a property it can build on.
            TextWindow::SplitCharacter => {
                return Ok(CasView {
                    meta: render_meta(
                        obj,
                        total,
                        &format!("bytes {start}..{end} of {total}"),
                        Some(
                            "these bytes begin or end inside a multi-byte character, so \
                             they are base64 rather than text — widen `length` or move \
                             `offset` to a character boundary to read this span as text",
                        ),
                    ),
                    body: Body::Base64(encode(&obj.bytes[start..end])),
                })
            }
            // Not UTF-8 at all. Hand back the exact bytes asked for, base64 — a lossy
            // decode would serve different content than was stored, and the base64 form
            // is itself the honest "treat this as binary" signal.
            TextWindow::NotText => {}
        }
    }

    Ok(CasView {
        meta: render_meta(
            obj,
            total,
            &format!("bytes {start}..{end} of {total}"),
            None,
        ),
        body: Body::Base64(encode(&obj.bytes[start..end])),
    })
}

fn encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The metadata block that leads every response. One `key: value` per line — cheap to
/// read, cheap to parse — with the optional lines simply absent rather than empty.
fn render_meta(obj: &CasObject, total: usize, served: &str, note: Option<&str>) -> String {
    let mut out = format!(
        "digest: {digest}\nuri: {prefix}{digest}\nmime: {mime}\nbytes: {total}\nbinary: {binary}",
        digest = obj.digest,
        prefix = crate::cas::CAS_URI_PREFIX,
        mime = obj.ext.mime(),
        binary = !obj.ext.is_textual(),
    );
    if let Some(label) = obj.label {
        out.push_str(&format!("\nlabel: {label}"));
    }
    if !obj.provenance_present {
        out.push_str("\nprovenance: absent (this object was stored without its sidecar)");
    }
    if let Some(path) = obj.path {
        out.push_str(&format!("\npath: {}", path.display()));
    }
    out.push_str(&format!("\nserved: {served}"));
    if let Some(note) = note {
        out.push_str(&format!("\nnote: {note}"));
    }
    out
}

/// What a byte window over textual content turned out to hold.
enum TextWindow<'a> {
    /// At least one whole character, starting exactly where the caller asked and ending
    /// at `to` — which may be earlier than the window's end, because the tail can cut a
    /// character short.
    Whole { to: usize, text: &'a str },
    /// The window cannot produce text starting where the caller asked: it begins inside a
    /// multi-byte character, or it starts one it has no room to finish. **Never an empty
    /// [`TextWindow::Whole`]** — see [`plan`] for why that difference is the whole bug.
    SplitCharacter,
    /// Not UTF-8 at all, whatever the extension claims.
    NotText,
}

/// The whole-character UTF-8 text at `[start, end)`, always **beginning at `start`**.
///
/// Only the tail is ever trimmed. An earlier version also stepped `start` forward off a
/// leading continuation byte, which looks like the symmetric courtesy and is silent data
/// loss: the skipped bytes appear in no response, so a caller paging by the ranges it is
/// handed reassembles an object with holes in it. A window that cannot begin where it was
/// asked to begin is [`TextWindow::SplitCharacter`] instead, and `plan` serves those exact
/// bytes as base64 — imperfect, but complete and escapable.
fn text_window(bytes: &[u8], start: usize, end: usize) -> TextWindow<'_> {
    // A continuation byte is a place no decoder can start reading from.
    if (bytes[start] & 0xC0) == 0x80 {
        return TextWindow::SplitCharacter;
    }
    let to = match std::str::from_utf8(&bytes[start..end]) {
        Ok(_) => end,
        // `error_len: None` means the input ended mid-character — the window's tail, not
        // corruption. Serve up to the last whole character and leave the rest for the
        // next page.
        Err(e) if e.error_len().is_none() => start + e.valid_up_to(),
        Err(_) => return TextWindow::NotText,
    };
    // The window starts a character it has no room to finish.
    if to <= start {
        return TextWindow::SplitCharacter;
    }
    match std::str::from_utf8(&bytes[start..to]) {
        Ok(text) => TextWindow::Whole { to, text },
        Err(_) => TextWindow::NotText,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    const TEXT_DIGEST: &str = "abababababababababababababababababababababababababababababababab";
    const PNG_DIGEST: &str = "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";

    fn text_obj<'a>(bytes: &'a [u8], label: Option<&'a str>) -> CasObject<'a> {
        CasObject {
            digest: TEXT_DIGEST,
            ext: Extension::Txt,
            bytes,
            label,
            provenance_present: true,
            path: None,
        }
    }

    fn png_obj(bytes: &[u8]) -> CasObject<'_> {
        CasObject {
            digest: PNG_DIGEST,
            ext: Extension::Png,
            bytes,
            label: None,
            provenance_present: true,
            path: None,
        }
    }

    /// **Metadata leads on every response, including the one that carries nothing else.**
    /// `length: 0` is the cheap HEAD: what is this, how big, what is it called, where does
    /// it live — for a few dozen tokens instead of the object.
    #[test]
    fn length_zero_is_metadata_only() {
        let body = "x".repeat(5000);
        let obj = text_obj(body.as_bytes(), Some("the inventory"));
        let view = plan(&obj, 0, Some(0), Delivery::Rendered).expect("a HEAD is always legal");
        assert_eq!(view.body, Body::None, "no content at length 0");
        assert!(view.meta.contains("5000"), "the total leads: {}", view.meta);
        assert!(
            view.meta.contains("text/plain"),
            "the mime leads: {}",
            view.meta
        );
        assert!(
            view.meta.contains("the inventory"),
            "the label leads when there is one: {}",
            view.meta
        );
        assert!(
            view.meta.contains("binary: false"),
            "and whether it is binary: {}",
            view.meta
        );
    }

    /// A sidecar with no label simply omits the line — never an empty or invented one.
    #[test]
    fn a_missing_label_is_omitted_not_faked() {
        let obj = text_obj(b"hi", None);
        let view = plan(&obj, 0, Some(0), Delivery::Rendered).unwrap();
        assert!(
            !view.meta.contains("label:"),
            "no label line without a label: {}",
            view.meta
        );
    }

    /// **The default read is a window, not the object.** An omitted `length` serves
    /// `DEFAULT_READ_BYTES` from the offset and says so, with the total beside it, so the
    /// caller decides whether to page rather than discovering the size by paying for it.
    #[test]
    fn an_omitted_length_serves_the_default_window_and_names_the_total() {
        let total = DEFAULT_READ_BYTES * 3;
        let body = "y".repeat(total);
        let obj = text_obj(body.as_bytes(), None);
        let view = plan(&obj, 0, None, Delivery::Rendered).expect("a default read is legal");
        match &view.body {
            Body::Text(t) => assert_eq!(
                t.len(),
                DEFAULT_READ_BYTES,
                "exactly the default window, not the whole object"
            ),
            other => panic!("a textual object serves text, got {other:?}"),
        }
        assert!(
            view.meta.contains(&total.to_string()),
            "the total is in the metadata so paging is informed: {}",
            view.meta
        );
    }

    /// Paging: a second call with an offset picks up where the first stopped, and the
    /// served range says so.
    #[test]
    fn offset_pages_through_a_large_object() {
        let total = DEFAULT_READ_BYTES + 100;
        let body: String = (0..total)
            .map(|i| ((i % 26) as u8 + b'a') as char)
            .collect();
        let obj = text_obj(body.as_bytes(), None);

        let first = plan(&obj, 0, None, Delivery::Rendered).unwrap();
        let second = plan(&obj, DEFAULT_READ_BYTES, None, Delivery::Rendered).unwrap();
        let (Body::Text(a), Body::Text(b)) = (&first.body, &second.body) else {
            panic!("both pages are text");
        };
        assert_eq!(a.len(), DEFAULT_READ_BYTES);
        assert_eq!(
            b.len(),
            100,
            "the tail is what is left, not a padded window"
        );
        assert_eq!(
            format!("{a}{b}"),
            body,
            "the two pages reassemble the object exactly"
        );
    }

    /// An offset past the end is an empty range, not an error: a caller paging a loop
    /// discovers the end by reading past it, and a hard failure there would make that
    /// normal move look like a fault.
    #[test]
    fn an_offset_past_the_end_is_an_empty_range_not_an_error() {
        let obj = text_obj(b"short", None);
        let view = plan(&obj, 9_999, None, Delivery::Rendered).expect("past the end is legal");
        assert_eq!(view.body, Body::None, "nothing left to serve");
        assert!(
            view.meta.contains('5'),
            "the total still leads, so the caller learns where the end was: {}",
            view.meta
        );
    }

    /// A `length` past the ceiling is refused, naming the ceiling and the way forward.
    /// Clamping instead would hand back less than was asked for while the caller's next
    /// offset assumed otherwise.
    #[test]
    fn a_length_past_the_ceiling_is_refused_with_the_ceiling_and_the_recovery() {
        let obj = text_obj(b"small", None);
        let err = plan(&obj, 0, Some(MAX_READ_BYTES + 1), Delivery::Rendered).expect_err("past the ceiling");
        assert!(
            err.contains(&MAX_READ_BYTES.to_string()),
            "the refusal names the ceiling: {err}"
        );
        assert!(
            err.to_lowercase().contains("offset"),
            "and the recovery is paging: {err}"
        );
        plan(&obj, 0, Some(MAX_READ_BYTES), Delivery::Rendered).expect("exactly at the ceiling is fine");
    }

    /// Textual content arrives as text a caller can use, never base64 it must decode.
    #[test]
    fn textual_content_arrives_as_text() {
        let obj = text_obj(b"line one\nline two\n", None);
        let view = plan(&obj, 0, None, Delivery::Rendered).unwrap();
        assert_eq!(view.body, Body::Text("line one\nline two\n".to_string()));
    }

    /// A binary range is base64 of **exactly** the bytes the metadata names — no padding,
    /// no re-encoding of a neighbouring window.
    #[test]
    fn a_binary_range_is_base64_of_exactly_those_bytes() {
        let bytes: Vec<u8> = (0u8..=255).collect();
        let obj = png_obj(&bytes);
        let view = plan(&obj, 10, Some(20), Delivery::Rendered).unwrap();
        assert_eq!(view.body, Body::Base64(b64(&bytes[10..30])));
    }

    /// **A base64 dump is never a default.** A binary object with no range asked for
    /// serves metadata alone — the caller either asks for a range or, in disk mode, reads
    /// the path. Sixty-four kilobytes of base64 PNG helps neither a person nor a model.
    #[test]
    fn a_binary_object_serves_no_body_unless_a_range_is_asked_for() {
        let big = vec![7u8; INLINE_IMAGE_MAX_BYTES + 1];
        let view = plan(&png_obj(&big), 0, None, Delivery::Rendered).unwrap();
        assert_eq!(
            view.body,
            Body::None,
            "an oversize image is metadata only, not a base64 wall"
        );
        assert!(
            view.meta.contains("binary: true"),
            "and the metadata says why: {}",
            view.meta
        );
    }

    /// **Small images take one hop to the eye.** No range asked for, size under the
    /// threshold: the whole image comes back as a content block hosts render straight to
    /// a vision model.
    #[test]
    fn a_small_image_with_no_range_comes_back_as_an_image_block() {
        let bytes = vec![9u8; 1024];
        let view = plan(&png_obj(&bytes), 0, None, Delivery::Rendered).unwrap();
        assert_eq!(
            view.body,
            Body::Image {
                data: b64(&bytes),
                mime: "image/png",
            }
        );
        assert!(
            view.meta.contains("1024"),
            "the metadata still leads: {}",
            view.meta
        );
    }

    /// An explicit range beats the image path even for a small image: the caller asked
    /// for bytes, so it gets bytes.
    #[test]
    fn an_explicit_range_on_a_small_image_returns_base64_not_an_image_block() {
        let bytes = vec![9u8; 1024];
        let view = plan(&png_obj(&bytes), 0, Some(16), Delivery::Rendered).unwrap();
        assert_eq!(view.body, Body::Base64(b64(&bytes[..16])));
    }

    /// Disk mode puts the real path in the metadata, so a caller holding a shell can go
    /// direct instead of paging megabytes through a context window. Memory mode has no
    /// file and says nothing.
    #[test]
    fn the_path_is_in_the_metadata_only_when_there_is_a_file() {
        let bytes = b"content";
        let on_disk = CasObject {
            path: Some(Path::new("/var/lib/kaibo/cas/ab/cd/abcd.txt")),
            ..text_obj(bytes, None)
        };
        assert!(
            plan(&on_disk, 0, Some(0), Delivery::Rendered)
                .unwrap()
                .meta
                .contains("/var/lib/kaibo/cas/ab/cd/abcd.txt"),
            "disk mode names the file"
        );
        assert!(
            !plan(&text_obj(bytes, None), 0, Some(0), Delivery::Rendered)
                .unwrap()
                .meta
                .contains("path:"),
            "memory mode has no file to name"
        );
    }

    /// The byte range a response says it served, parsed out of the metadata — what a
    /// mechanical pager reads to pick its next `offset`.
    fn served_range(meta: &str) -> Option<(usize, usize)> {
        let line = meta.lines().find(|l| l.starts_with("served: bytes "))?;
        let span = line
            .trim_start_matches("served: bytes ")
            .split(" of ")
            .next()?;
        let (a, b) = span.split_once("..")?;
        Some((a.parse().ok()?, b.parse().ok()?))
    }

    /// **A missing sidecar is its own state, and the metadata says so.**
    ///
    /// Three things a caller can be looking at, and they are not the same: an object whose
    /// record carries a label, one whose record carries none, and one with no record at
    /// all. The last is real — it is exactly what `CasError::ProvenanceNotRecorded` mints
    /// and what the extension-probe fallback serves — and silence about it would let a
    /// caller read "no label" as "nothing was written down here", which is a different
    /// and much weaker claim.
    ///
    /// A `label:` line when there is a label, a `provenance:` line when there is no
    /// record, and neither when the record simply has no label to give.
    #[test]
    fn the_metadata_distinguishes_no_label_from_no_provenance_at_all() {
        let bytes = b"content";

        let labeled = text_obj(bytes, Some("the inventory"));
        let m = plan(&labeled, 0, Some(0), Delivery::Rendered).unwrap().meta;
        assert!(m.contains("label: the inventory"), "{m}");
        assert!(
            !m.contains("provenance:"),
            "a present record says nothing: {m}"
        );

        let unlabeled = CasObject {
            label: None,
            ..text_obj(bytes, None)
        };
        let m = plan(&unlabeled, 0, Some(0), Delivery::Rendered).unwrap().meta;
        assert!(!m.contains("label:"), "no label, no line: {m}");
        assert!(
            !m.contains("provenance:"),
            "a record that simply has no label is not a missing record: {m}"
        );

        let orphaned = CasObject {
            provenance_present: false,
            ..text_obj(bytes, None)
        };
        let m = plan(&orphaned, 0, Some(0), Delivery::Rendered).unwrap().meta;
        assert!(
            m.contains("provenance: absent"),
            "a missing record is stated: {m}"
        );
        assert!(!m.contains("label:"), "and still invents no label: {m}");
    }

    /// **A window that holds no complete character must still advance.**
    ///
    /// `é` is two bytes. Asking for one of them used to trim the window to nothing and
    /// report `served: bytes 0..0` — a caller resuming at the endpoint it was handed reads
    /// the same byte forever. Serving zero bytes while claiming a range is worse than
    /// serving something imperfect: a pager cannot detect it and cannot escape it.
    ///
    /// So the exact bytes asked for come back as base64, the served range covers them, and
    /// a note says why they are not text. The cursor moves; the caller can widen or
    /// realign; nothing loops.
    #[test]
    fn a_window_holding_no_whole_character_advances_over_the_bytes_it_was_given() {
        let obj = text_obj("é".as_bytes(), None);
        let view = plan(&obj, 0, Some(1), Delivery::Rendered).expect("a legal range");
        assert_eq!(
            view.body,
            Body::Base64(b64(&[0xC3])),
            "the exact byte asked for, not an empty string"
        );
        assert_eq!(
            served_range(&view.meta),
            Some((0, 1)),
            "and the range advances past it: {}",
            view.meta
        );
        assert!(
            view.meta.contains("note: "),
            "with a note saying why it is not text: {}",
            view.meta
        );
    }

    /// The same, trimmed at *both* edges: a window entirely inside one 4-byte character is
    /// all continuation bytes, so neither edge can be salvaged.
    #[test]
    fn a_window_inside_one_character_advances_over_the_bytes_it_was_given() {
        let obj = text_obj("😀".as_bytes(), None); // F0 9F 98 80
        let view = plan(&obj, 1, Some(2), Delivery::Rendered).expect("a legal range");
        assert_eq!(view.body, Body::Base64(b64(&[0x9F, 0x98])));
        assert_eq!(
            served_range(&view.meta),
            Some((1, 3)),
            "the cursor advances over exactly what was asked for: {}",
            view.meta
        );
    }

    /// **The property a pager actually relies on: the cursor always moves.** Walk a
    /// multi-byte corpus at every small window size; each response's served range must
    /// start where the last one ended and end strictly later, until the object runs out.
    /// The old trim-to-empty behavior hangs this test rather than failing an assertion,
    /// so it carries its own iteration bound.
    #[test]
    fn paging_a_multibyte_corpus_always_advances_to_eof() {
        let corpus = "aé漢😀b\nzこんにちは😀é";
        let obj = text_obj(corpus.as_bytes(), None);
        let total = corpus.len();

        for window in 1..=6 {
            let mut cursor = 0usize;
            let mut steps = 0;
            while cursor < total {
                steps += 1;
                assert!(
                    steps <= total + 2,
                    "window {window}: paging did not terminate — stuck at {cursor}"
                );
                let view = plan(&obj, cursor, Some(window), Delivery::Rendered).expect("a legal range");
                let (from, to) = served_range(&view.meta)
                    .unwrap_or_else(|| panic!("window {window} at {cursor}: {}", view.meta));
                assert_eq!(
                    from, cursor,
                    "window {window}: the range starts where asked"
                );
                assert!(
                    to > cursor,
                    "window {window} at {cursor}: served {from}..{to} does not advance"
                );
                assert!(to <= total, "window {window}: {to} is past the object");
                cursor = to;
            }
            assert_eq!(
                cursor, total,
                "window {window}: paging lands exactly on the end"
            );
        }
    }

    /// Reassembly is the other half of the contract: whatever form each page came back in,
    /// the bytes a caller collects are the object.
    #[test]
    fn pages_of_a_multibyte_corpus_reassemble_the_original_bytes() {
        use base64::Engine as _;
        let corpus = "aé漢😀b";
        let obj = text_obj(corpus.as_bytes(), None);
        let mut out: Vec<u8> = Vec::new();
        let mut cursor = 0;
        // Bounded like its sibling: a non-advancing cursor is the failure under test, and
        // a test that hangs on the bug reports nothing.
        for _ in 0..corpus.len() + 2 {
            if cursor >= corpus.len() {
                break;
            }
            let view = plan(&obj, cursor, Some(3), Delivery::Rendered).unwrap();
            let (_, to) = served_range(&view.meta).unwrap();
            match &view.body {
                Body::Text(t) => out.extend_from_slice(t.as_bytes()),
                Body::Base64(d) => out.extend_from_slice(
                    &base64::engine::general_purpose::STANDARD
                        .decode(d)
                        .expect("valid base64"),
                ),
                other => panic!("unexpected body {other:?}"),
            }
            cursor = to;
        }
        assert_eq!(
            cursor,
            corpus.len(),
            "paging terminated at the end, not by the bound"
        );
        assert_eq!(out, corpus.as_bytes(), "the pages reassemble the object");
    }

    /// A zero-byte textual object says it is empty — it used to fall through to the
    /// "this object is binary" wording, which is simply not true of an empty `.txt`.
    #[test]
    fn an_empty_object_says_it_is_empty() {
        let view = plan(&text_obj(b"", None), 0, None, Delivery::Rendered).unwrap();
        assert_eq!(view.body, Body::None);
        assert!(
            view.meta.contains("empty"),
            "an empty textual object says so: {}",
            view.meta
        );
        assert!(
            !view.meta.contains("binary, so pass"),
            "and does not claim to be binary: {}",
            view.meta
        );
    }

    /// A window that lands mid-character serves the largest whole-character range inside
    /// it and reports what it served. Paging UTF-8 by byte offset splits multi-byte
    /// characters constantly, and neither a lossy decode (different bytes than stored) nor
    /// a base64 fallback for the whole window (unreadable) is an acceptable answer.
    #[test]
    fn a_window_splitting_a_multibyte_character_trims_to_a_boundary() {
        // Each `é` is two bytes, so a 3-byte window from 0 splits the second one.
        let body = "ééé";
        let obj = text_obj(body.as_bytes(), None);
        let view = plan(&obj, 0, Some(3), Delivery::Rendered).unwrap();
        assert_eq!(
            view.body,
            Body::Text("é".to_string()),
            "the split character is left for the next page"
        );
        assert!(
            view.meta.contains("0..2") || view.meta.contains("0-2"),
            "and the served range is the trimmed one, so the next offset is right: {}",
            view.meta
        );
    }

    /// Bytes stored under a textual extension that are not UTF-8 at all fall back to
    /// base64 of the exact requested range — never a lossy decode, which would hand back
    /// different bytes than were stored.
    #[test]
    fn undecodable_textual_bytes_fall_back_to_base64() {
        let bytes: &[u8] = &[0x66, 0x6f, 0x6f, 0xff, 0xfe, 0x62, 0x61, 0x72];
        let obj = text_obj(bytes, None);
        let view = plan(&obj, 0, None, Delivery::Rendered).unwrap();
        assert_eq!(view.body, Body::Base64(b64(bytes)));
    }
}
