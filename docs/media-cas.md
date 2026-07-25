# Media CAS + image generation — living design doc

Delete this file when the work ships; the reasoning lands in `docs/devlog.md`.

## Reopening image generation (discussed 2026-07-25, Amy + Claude)

Image generation was **built and then deliberately removed** on 2026-06-28
(`986806f`, ~1,700 lines gross). The recorded reason (`docs/issues.md:27-31`) was
*identity*, not safety:

> Image output used none of kaibo's differentiators (read-only sandbox, cross-model
> code reasoning).

The safety payoff — read-only becoming unconditional — was framed as the *consequence*
of that call, not its cause. Any reopening has to answer the identity question on its
own terms, which is what this section does.

**The identity answer (Amy, 2026-07-25):** kaibo is *a sidekick agent bringing other
model capabilities* — the models supercharge what we can do here. That is a **wider
thesis** than "cross-model code reasoning," and it supersedes the narrower framing the
June decision was made under. Recorded explicitly so this isn't a quiet drift: the
thesis moved, and the June conclusion doesn't survive the move.

**What else changed since June, materially:**

- **Persistence landed.** In June kaibo had *zero* on-disk state, so a write path
  genuinely broke an unconditional claim. Today `src/store.rs` holds a turso db at a
  fixed XDG state path, with `SessionStore::open` refusing any path that resolves
  inside an allowed tree (`tests/store.rs`). The claim is already amended, carefully.
- **The operator/model-team line is now explicit** (`CLAUDE.md:69-79`). It didn't
  exist in June. A CAS sits cleanly on the *operator* side.
- **`tests/no_write_path.rs` anticipates exactly this**: *"A future deliberate
  capability that records an artifact is a conscious exception updated here in the same
  change and its review, never silently."* The guard was written to be opened
  consciously — this is that.

**A CAS is not a revival of what was removed.** The bulk of the deleted 1,700 lines was
*out-dir* machinery: the sandbox out-dir mount, the allowed-set widening, `OutDirAttach`
in attachment resolution, the `DefaultOutDir` shared-temp classifier, the
`--out-dir`/`--no-out-dir-read` flags. A content-addressed store needs **none of it** —
nothing is mounted, nothing is model-reachable, the allowed set does not move. This is a
much smaller thing wearing none of the parts that made the old design heavy.

## The design

A small content-addressed store next to the existing persistence, holding generated
artifacts as opaque bytes.

- **The address is the checksum.** No caller-supplied destination path anywhere in the
  API — the path is *derived* from the content. A caller structurally cannot aim it, so
  the whole path-traversal / clobber-the-user's-file class does not exist. This is the
  load-bearing safety property; it is a shape, not a policy we enforce.
- **Write-only, `create_new` (O_EXCL), `sync_all`'d.** No unlink, no truncate, no rename.
  Same hash ⇒ same bytes ⇒ the write is a no-op, so even a collision cannot clobber.
  Copy-on-write for "edits" falls out for free: new content is a new address.
- **Verify before trusting a path exists.** A crash between `create_new` and a completed
  `write_all` can leave a truncated object sitting at the exact path a later write of the
  same content would treat as "already there" — `sync_all` narrows that window, it can't
  close it. So `get` recomputes the SHA-256 of every byte it reads and refuses to return
  anything that doesn't match the digest it was asked for (`Err(CasError::Corrupt)`,
  never a silent `None`), and `put`'s dedup path performs the same check before reporting
  success on an `AlreadyExists`. This is affordable because the whole object is always
  slurped into memory before any byte reaches the caller — see `src/cas.rs`'s module doc
  ("Integrity") for why a future streaming read could not inherit this guarantee for
  free.
- **We stay out of the GC game.** The user backs it up (`rsync`, `restic`) or prunes it
  (`find -mtime +N`) if they want to. For that to be *honest* rather than a punt, each
  object gets a provenance sidecar (prompt, model, cast, timestamp) so a user can prune
  intelligently instead of guessing at opaque bytes.
- **Bounded writes, loud refusal.** A configurable soft cap **refuses** new writes when
  reached; it never evicts. Evicting would quietly make "write-only" a lie, and disk-full
  is exactly the crash-over-corrupt case.
- **Separate directory from the session db.** Non-negotiable: corruption recovery for the
  db is an operator moving the file aside by hand, and that motion must never be able to
  take paid artifacts with it.

### Why files, not turso blobs

Checked turso 0.7.0 directly. `Value::Blob(Vec<u8>)` (`turso-0.7.0/src/value.rs:11`) is
the entire surface — **no incremental/streaming blob I/O** (grepped; the only
`incremental` hits are unrelated MVCC sync lines). So every read and write would
materialize the whole image in memory, the bytes would land in overflow page chains that
grow the db with no working VACUUM story, and paid content would live inside the one
file whose documented lethal hazard is silent loss of acknowledged writes on a mixed
MP/non-MP open. It would also destroy the `rsync`/`find` story that makes user-owned GC
possible. Files win on every axis.

### Hash: sha2, not blake3

`sha2 0.10.9` is **already in our lock** — pure Rust, zero new deps, zero C. `blake3` is
not in the tree and carries `[build-dependencies.cc]` (`blake3-1.8.5/Cargo.toml:134`);
its `pure` feature does disable the C/asm build (`build.rs:346,354,368` branch on
`is_pure()`), but that's a new dependency and a build-shape decision to buy a speed edge
that is invisible at image sizes (~1ms either way, both 256-bit). Decision (Amy): **sha2**
— easier to stay pure, keeps the binaries simple.

## Invariant impact

1. **The four levers are untouched.** No kaish write path, no mount, no change to the
   allowed set. The shell still writes nothing.
2. **A second blessed write site.** `tests/no_write_path.rs` currently asserts
   `blessed_marker_appears_exactly_once`. The CAS needs its own marker, so that test
   loosens to an explicit two-entry allowlist. This is a **visible ratchet loosening** and
   should be loud — it lands in the same PR as the capability and its review, per that
   file's own rule.
3. **The headline claim gets rewritten again**, precisely. It is what other people's
   agents rely on, so the sentence matters more than the code.

### Decided: no CAS access from kaish

Considered exposing the CAS read-only to the model team. Read access breaches nothing
about *writes*, but it collides with the operator/model-team invariant
(`CLAUDE.md:69-79`): the CAS is kaibo state, kaibo state spans projects, and surfacing it
to a cast is the cross-project leak that invariant forbids.

A CAS nearly dissolves the objection on its own — **the address is the capability**. You
can only read what you already know the digest of, and knowing the digest means someone
handed it to you. The leak surface collapses to exactly one thing: **enumeration**. A
browsable mount lets `ls` walk every artifact from every project; open-by-digest-only
leaks nothing.

**Decision (Amy, 2026-07-25): don't mount it.** Ordinary filesystem access can reach the
CAS if the caller should have it at all. If the model team ever needs to see a generated
image, that is `view_image` resolving a digest to bytes — a lookup, never a directory
kaish can walk. If a mount is ever revisited, **non-enumerable** is the property to
defend, not read-only.

## kaish binary transport

Amy's direction: use kaish's now-complete binary data transport where we hook up.

**It exists** — `ExecResult::out_bytes()`/`set_out_bytes()`, and `output_limit.rs:177-217`
measures and spills binary payloads by *raw* bytes rather than text.

**kaibo currently refuses it, deliberately.** `KaishOutput` is
`{code, stdout: String, stderr: String}` (`sandbox.rs:318-321`) and `from_result`
(`:338-357`) detects `out_bytes()` and drops the payload rather than let `text_out()`
lossy-decode it to mojibake and hand the model silent garbage with exit 0. That refusal
is correct and stays the **default**; wiring binary transport means growing `KaishOutput`
a bytes variant *alongside* it, keeping the anti-mojibake guard everywhere that isn't an
explicit binary channel. `KaishOutput` must stay `Send` (worker boundary).

**Noted while reading:** that refusal message advises the model to "redirect to a file,"
which reads oddly now that there is no write path. Re-read it against what kaish can
actually do today (MemoryFs at `/` may make it half-true, which is worse than wrong).

## Testing

Per Amy: thorough, with e2e especially where persistence and binary translations meet.

The sharp edge is **byte-exactness across every translation boundary**: provider base64 →
bytes → digest → disk → read back → digest again → into model context as an image part.
An e2e asserting the digest is stable at both ends catches the entire class. This is not
theoretical — the existing `out_bytes`/`text_out` trap (`sandbox.rs:330-337`) proves we
can lose bytes silently at exactly this kind of seam.

Failing-first tests the boundary needs:

- The CAS API has **no destination-path parameter** (compile-level, plus a schema test).
- `create_new` never clobbers; same content twice is a no-op, not a rewrite.
- kaish cannot reach the store (extends the existing sandbox suite).
- The CAS path canonicalizes **outside** every allowed read tree (mirrors
  `tests/store.rs`'s containment test for the db).
- The soft cap **refuses** rather than evicts.
- Round-trip digest stability (the e2e above).
- **`get` refuses to hand back unverified bytes.** A poisoned object (content that does
  not hash to the address it's stored at — the truncated-write crash scenario above,
  reproduced in tests by writing directly at the object path) surfaces
  `Err(CasError::Corrupt)`, not silently-wrong bytes and not a false `None`. A never-written
  digest still returns `Ok(None)`, and a healthy round-trip still returns `Ok(Some(bytes))`.
- **`put`'s dedup path refuses to report false success.** Re-`put`ting content whose slot
  was poisoned out-of-band returns `Err(CasError::Corrupt)` rather than `Ok` — a caller
  must never believe its bytes were saved when they were silently discarded onto bad data.
- An object that exists but can't be read at all (not just wrong content) surfaces
  `Err(CasError::Io)`, distinct from both of the above — none of "missing," "unreadable,"
  and "wrong content" are allowed to look the same to a caller.

## Separable now — the "disposable" language

`store.rs`'s module doc calls the db *"reconstructible-or-disposable... corruption is
handled by deleting the file and starting over."* Traced its origin: `signoff.md`, during
the turso evaluation — *"Beta risk acceptable: store data is low-stakes and
reconstructible-or-disposable."* That was a **risk-acceptance argument for choosing an
engine**, and it got promoted into the module doc as though it were data-stewardship
policy.

It is wrong today regardless of the CAS: sessions hold `(question, answer)` pairs, which
*are* paid model output, so the sanctioned recovery path already discards content the user
paid for. New stance (Amy, 2026-07-25): **we strive to keep the user's data at most
costs.**

The sharper axis for how conservative to be is **reproducibility**, not payment. A lost
session costs a re-ask and returns something equivalent. A lost image may never come back
— sampling, seeds nobody kept, a retired model. So the CAS warrants a more conservative
deletion story than the db, not because it cost more but because it is less recoverable.

**This should ship as its own small PR**, independent of the CAS decision.

## Open

- Provider wiring for Stability (key at `~/.stability-key.txt`). rig has the *shape* —
  `ImageGenerationModel` / `ImageGenerationRequest{prompt,width,height,additional_params}`
  / `ImageGenerationResponse{image: Vec<u8>}` — behind `feature = "image"`
  (`rig-core/src/lib.rs:159`), which `986806f` turned off. **No Stability provider**
  (impls: openai, xai, azure, huggingface, hyperbolic), so we write it, templated on
  `providers/xai/image_generation.rs` (~120 lines). Stability v2beta is
  `multipart/form-data` in and raw image bytes out, *not* OpenAI-shaped — confirm against
  current docs at implementation time rather than from memory. `width`/`height` vs
  Stability's `aspect_ratio` rides in `additional_params`.
- Turning `feature = "image"` back on is worth it beyond Stability: an `image` **role** in
  a cast could point at openai / xai / huggingface for free.
- Config surface to re-add: `986806f` reduced `ModelRole` to Explorer/Synth and made an
  `image =` cast slot a loud `deny_unknown_fields` load error.
- Check `reqwest`'s `multipart` feature against the TLS invariant (`cargo tree -i
  aws-lc-rs` must stay empty; expected clean — it pulls `mime_guess`, not crypto).
- The exact replacement wording for the headline invariant claim.
