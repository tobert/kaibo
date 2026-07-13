# Releasing kaibo — pipeline plan & living record

kaibo ships as a single static-ish binary per platform. This document is the
*living* plan for how we build and publish those binaries — the sequence of PRs that
gets us from "a workflow that has never fired" to "signed, attested releases an agent
operator can trust." Update it as PRs land; it is the durable map the conversation
can't reconstruct.

> **Gating note.** This whole pipeline is **parked behind the pre-1.0 work**. The hard
> gate on **widening the audience** (announcing, promoting) is that kaibo must be **super
> easy to install and use** first — for a tool whose users are *other people's agents*,
> install friction is fatal. So the container + `/reconfigure` UX below isn't side-prep,
> it's *on the critical path to going wide*. The engineering PRs (and even a `v*` tag) can
> land before then; the go-wide moment waits on the ease bar. The value of writing the plan
> now is to *settle the decisions* so we don't re-litigate them, and to let the PRs land
> incrementally (each independently reviewable) as the runway clears. Nothing here forces a
> release.

---

## Where we are now

`.github/workflows/release.yml` already exists: a hand-rolled 5-target matrix that
builds **natively per platform** — Linux `x86_64`/`aarch64` musl (fully static via
`cargo-zigbuild`), macOS `x86_64`/`aarch64` (native `cargo build` on `macos-latest`),
Windows `x86_64` MSVC (native, `+crt-static`) — then packages `tar.gz`/`zip`, writes
sha256 sidecars, and publishes on a `v*` tag. It is a good
baseline (the static-musl + ring/no-aws-lc groundwork is real), but it does no signing,
no provenance, no container image, no package channels, and it pins its actions by
floating tag.

**Baseline run — 2026-07-05, the first fire ever** (`workflow_dispatch` from `main`,
run 28745954560): **all five legs green first try**, ~8.5 min wall, artifacts + sha256
sidecars for every target, publish job correctly skipped (tag-gated). The x86_64-musl
artifact verified locally: checksum OK, `statically linked` / `not a dynamic
executable`, `kaibo --version` runs. Observed for the dial-in PRs: Node 20 deprecation
warnings on `checkout@v4`/`upload-artifact@v4` (bump majors while SHA-pinning) and a
macos-latest → macOS 26 migration notice. The pre-flight review's portability worries
did **not** bite (macOS runners ship coreutils' `sha256sum`; bare `pip` exists) — still
prefer the portable forms (`shasum -a 256` isn't needed, but `python3 -m pip` is free)
when touching those lines.

**PR 2 realized — 2026-07-05, same day** (dial-in slices #60/#61/#62, each validated by
a branch dispatch + cross-family reviewed): SHA-pinned actions (digests independently
verified), least-privilege per-job permissions, branch-safe artifact names (the
`ref_name` slash footgun fired on the first branch dispatch and was fixed in-slice),
aarch64 leg on `ubuntu-24.04-arm` with per-leg `--version` smoke + musl `ldd` static
assert, reproducible archives (`SOURCE_DATE_EPOCH`, gtar `--sort`/`--mtime` | `gzip -n`,
pinned zip mtimes — teeth-tested byte-identical locally), prebuilt zigbuild. Also landed
alongside: the repo's **first CI workflow** (#64 — offline suite, clippy `-D warnings`,
a `cargo tree` TLS-invariant tripwire) and **`v0.2.0-rc.1`, the first-ever release**
(#63 + tag): the publish job ran for the first time, produced a correctly-**prerelease**
GitHub release with all 10 assets, and the end-to-end user path verified — download,
checksum OK, fully static, binary reports `kaibo 0.2.0-rc.1`. The rc exists to prove the
tag→release leg *before* PR 3, so the real v0.2.0 is born signed.

**PR 3 realized — 2026-07-13.** Signing/provenance/SBOM live entirely in the tag-gated
publish job, so a `workflow_dispatch` smoke run never mints an OIDC identity (build legs
keep `contents: read`; the publish job adds `id-token: write` + `attestations: write`).
The layout, decided with Amy: **one signed aggregate `checksums.txt`** (cosign keyless —
verify once, `sha256sum -c` covers any file it lists; one signature shape, the
self-contained `.sigstore.json` bundle, which verifies offline with cosign ≥ 3; the
per-artifact `.sha256` sidecars stay for the README's download one-liner),
**per-artifact SLSA provenance** via `actions/attest-build-provenance` (stored in
GitHub's attestation store — `gh attestation verify <file> -R tobert/kaibo`, zero extra
assets), and **one SPDX SBOM from `Cargo.lock`** (a bare Rust binary carries nothing for
syft to read; `cargo-auditable` per-binary SBOMs are a tracked follow-up in
`docs/issues.md`). README gained a "Verify a download" section with the exact
invocations — including the identity flags keyless verification requires and
`--ignore-missing` so a single-file download checks clean. New pins
(`cosign-installer`, `attest-build-provenance`, `sbom-action`) digest-verified through
two independent paths. Validated live by **`v0.2.0-rc.2`** (fired the whole path first
try — SBOM → checksums → sign → attest → release, publish job 14s) with all three
README verification commands proven against its real assets: `gh attestation verify`
exit 0, `cosign verify-blob --bundle` Verified OK under the tag identity,
`sha256sum -c --ignore-missing` OK. The rc earned its keep by catching the one kink:
cosign v3 *ignores* the legacy `--output-signature`/`--output-certificate` flags in
bundle mode (warned, wrote nothing — the planned `.sig`/`.pem` pair never shipped), so
the bundle became the only signature shape rather than pinning cosign back to v2 for a
format it deprecates; `v0.2.0-rc.3` validates the bundle-only invocation. **Next: PR 4
(ghcr image), and the real v0.2.0 — born signed.**

This doc is the *pipeline* side only. The operator-side checklist for actually cutting
a release (CHANGELOG retitle, kaish-kernel pin check, `docs/sandbox-probes.md` re-run,
`cargo tree -i aws-lc-rs` empty, musl `not a dynamic executable`) lives in CLAUDE.md
**Cutting a release** — the two reference each other so neither drifts.

## The decisions (settled 2026-06-25, with Amy)

The short version: **stay OSS and GitHub-native; do not adopt a release framework for
capability we already have.** The spine is the *existing native matrix*, hardened, plus
the free GitHub-native signing/attestation layer. Specifically:

1. **Keep the native matrix on GitHub-hosted runners.** Each platform builds on its own
   runner (native macOS, native Windows MSVC, Linux musl via zigbuild). Because we build
   natively, there is **no cross-compile** — so no Apple-SDK hack, and **Windows stays
   MSVC** (no ABI change, no invariant to reword). GitHub-hosted runners are fine; no
   self-hosted runner needed.
2. **Transparency payoff via GitHub-native tooling, no middleman:** `cosign` **keyless**
   signing (OIDC, no long-lived key to leak), **SLSA build provenance** via
   `actions/attest-build-provenance` (free, GitHub-native), and an **SBOM** via `syft`.
   None of this needs a release framework or a paid service.
3. **GoReleaser is optional, OSS-only, and later.** Reach for it *only if* package-manager
   channel fan-out (Homebrew + Scoop + Winget + nfpm) grows into real toil. **GoReleaser
   Pro is off the table** — at $165/yr it would buy us orchestration of a build it isn't
   doing (we're native-matrix) plus notarization we can wire ourselves. If GoReleaser
   ever enters, it's the free OSS version, as a back-half channel publisher.
4. **Ship the ghcr image as a first-class, early distribution path — not optional**
   (decided with Amy, 2026-06-25). The reviewers' "stdio + container is hostile" objection
   was scoped to a *host* agent hand-building `docker run -i` mounts; the **devcontainer**
   case flips it — inside a devcontainer everything is already containerized and mounts are
   declared in `devcontainer.json`, so a kaibo container is *idiomatic*. It also doubles as
   an **OS-enforced containment layer beneath kaibo's own read-only sandbox** (belt and
   suspenders): it doesn't replace the app-level boundary, it backstops it — limiting
   blast radius if the sandbox code has a bug, and giving the operator a place to shape
   kaibo's outbound-to-provider egress (controlled egress, *not* full isolation, or the
   provider calls die). Non-negotiables: **multiarch** (amd64+arm64, from the per-arch
   musl binaries we already build), **non-root by default** (`distroless:nonroot`), and
   **documented user/volume mapping**. The real friction is *not* `-i` — it's UID/volume
   mapping (podman `--userns=keep-id` vs docker `-u $(id -u):$(id -g)` against a read-only
   project mount), which the configurator pass resolves (see "Container UX" below).

## Why we revised (the investigation)

Earlier the same day we had chosen "adopt GoReleaser, single Linux runner." Two findings
flipped it; recording them so the reversal isn't mysterious later:

- **The macOS cross-compile that justified GoReleaser doesn't work for us.** GoReleaser's
  headline value is its cross-compile matrix (zigbuild every target from one runner). But
  `cargo-zigbuild` can't link an Apple-`darwin` target from Linux without the proprietary
  Apple SDK (zig can't bundle it — Apple's EULA), and kaibo specifically pulls
  `rustls-platform-verifier` → `security-framework`/`core-foundation`, which FFI into
  Apple frameworks. So macOS *must* build on a real Mac — which is exactly where
  GoReleaser stops building anything and becomes pure orchestration of archive/checksum/
  release steps we already have in ~10 lines of bash. We'd carry a framework for the one
  feature we can't use.
- **There is no service to outsource this to, and the reason is structural.** The most
  serious venture swing at "release engineering as a service" — **axo (axodotdev)**, Ashley
  Williams, VC-backed, 2022 — built exactly this (cargo-dist + hosted release pages +
  updater + analytics) and wound down; `axo.dev` is now a parked domain and the tool
  (`dist`) survives only community-maintained. The market keeps re-converging on
  OSS-tool-on-your-CI (GoReleaser, JReleaser, dist). *Why* the SaaS starves: the **build**
  needs your runners/toolchain, the **signing** needs *your* identity (a service holding
  it is the supply-chain single-point-of-trust the security-conscious refuse — Sigstore's
  keyless design exists precisely to avoid a key-holding middleman), and the **publish**
  step is thin git PRs. What's left to bill for is hosted pages + analytics — nice, not
  need. So **free OSS + GitHub-native is the industry equilibrium, and it happens to align
  exactly with kaibo's transparency/no-middleman values.**
- **A cross-family review hardened the specifics** (Gemini batch + DeepSeek consult, run
  *through kaibo's own tools* — dogfooding the review). Their validated findings are folded
  into the PRs below.

## Why public CI, hardened (not local builds)

Amy's instinct was that *no* public CI keeps kaibo off the radar of botnets hunting CI to
attack, and that local builds might feel more honest. The analysis landed the other way;
recording it so we don't reopen it:

- Mass-attack vectors hunt for workflows that run on **fork PRs** (free-runner mining) or
  leak **long-lived secrets**. A **tag-only + `workflow_dispatch`** release workflow with
  **no PAT** dodges both by shape.
- The real residual risk is a **compromised third-party action** (cf. the
  `tj-actions/changed-files` incident). Mitigation is cheap: **pin every `uses:` to a
  commit SHA** (PR 2).
- **Local builds fight the transparency goal.** A public CI log is a third-party-witnessed,
  reproducible record; a laptop is an opaque box whose provenance is "Amy says so." The
  modern answer to "why trust this binary" is **keyless provenance attestation**, only
  cheaply attainable from public CI with OIDC — local builds *structurally cannot* produce
  it. So public CI, hardened, maximizes transparency **and** minimizes realistic attack
  surface.

## Cross-family review findings (folded into the PRs)

Validated and actionable, with their home PR:

- **"C-free" is imprecise** (both reviewers). `ring` *does* compile C/asm (via `cc` /
  `zig cc`); the tree is free of *cmake/autotools/OpenSSL/aws-lc system-C*, not free of C.
  Tighten the wording in `release.yml` comments and the CLAUDE.md **Build & release** /
  **TLS** lines when PR 2/3 touches them. (No behavior change — the invariant holds; `cargo
  tree -i aws-lc-rs` stays empty.)
- **cosign keyless `verify-blob` fails without identity flags** (Gemini). A bare verify
  *cannot* work for keyless — PR 3 docs must show the exact
  `--certificate-identity "...release.yml@refs/tags/vX.Y.Z"` +
  `--certificate-oidc-issuer "https://token.actions.githubusercontent.com"`. Signing nobody
  can verify is theater.
- **Add an SBOM** (Gemini). `syft` → SPDX/CycloneDX alongside the signatures (PR 3). For a
  security-minded operator, "what's in it" is arguably more useful than "where it was built."
- **Reproducible archives** (Gemini). Set `mod_timestamp`/`SOURCE_DATE_EPOCH` from the commit
  date so the `tar.gz` is byte-stable (PR 2).
- **Linking ≠ running** (DeepSeek). The validation gate must *run* each built binary
  (`kaibo --version`) per target, not just `ldd`/`file` — a binary can link and still crash
  at startup. The native matrix already exercises each platform on its own runner, which is
  the right place to add the smoke run (PR 2).
- **Distroless TLS cert smoke test** (DeepSeek). `rustls-native-certs`/`openssl-probe` should
  find `/etc/ssl/certs/ca-certificates.crt` in `distroless/static` (it bundles CAs), with
  `webpki-root-certs` as the bundled fallback — but smoke-test an outbound HTTPS call in the
  image (PR 4).
- **Homebrew tap is a blocking external prereq** (DeepSeek). `tobert/homebrew-kaibo` must
  exist before PR 5 — a pre-PR-5 checklist item, not buried in prose.
- **Container `-i` is a silent failure mode** (both). Without stdin attached, the MCP server
  gets immediate EOF and exits 0 — the hardest failure to debug, and `distroless/static` has
  no shell to `docker exec` into. Document prominently (PR 4).

---

## The PR sequence

Each PR lands on its own branch in a **git worktree** (Amy's workflow for this series),
goes up as a PR, gets a **cross-family review**, and updates `CHANGELOG.md` where
user-facing. Ordered lowest-coupling first; signing after artifacts exist; channels last.

### PR 1 — this document
The living plan itself (`docs/releases.md`) + a `docs/issues.md` pointer. Settles the
decisions above. No pipeline change. *(You are reading the artifact.)*

### PR 2 — harden & extend the existing native matrix
No framework, no ABI change — sharpen what's already there.
- **Step 0 — DONE 2026-07-05: fired the workflow via `workflow_dispatch`.** All five
  legs green on the first run; results + observed follow-ups in "Where we are now".
  Since nothing broke, the rest of this PR lands as small dial-in slices: (a) SHA-pins +
  per-job permissions, (b) arm leg + smoke runs, (c) packaging polish (reproducible
  archives, `ref_name` slash sanitize, upload-glob cleanup, prebuilt zigbuild).
- **SHA-pin every `uses:`** to a commit digest (the `tj-actions` lesson); minimal
  `permissions` **per job** (build: `contents: read`; only the publish job keeps
  `contents: write`) and keep `tags: ["v*"]` + `workflow_dispatch`.
- **Move the `aarch64-unknown-linux-musl` leg to `ubuntu-24.04-arm`** (GitHub's arm64
  runners, free for public repos since early 2025 — postdates this plan's first draft).
  Still zigbuild→musl for the fully-static link; the point is the binary now smoke-runs
  on real arm64 hardware, closing the one target the smoke gate couldn't cover.
- **Add a `--version` smoke run** per target on its native runner (linking ≠ running;
  `kaibo --version` exists — clap `version`, verified 2026-07-05), and
  **`SOURCE_DATE_EPOCH`/`--mtime`** from the commit date for reproducible archives.
  Note: `macos-latest` is arm64 (macos-14+), so the `x86_64-apple-darwin` smoke runs
  under **Rosetta 2** — expected and fine; comment it in the workflow so nobody
  "fixes" the leg later.
- **Install `cargo-zigbuild` prebuilt** (a SHA-pinned installer action, e.g.
  `taiki-e/install-action`) instead of `cargo install --locked` compiling it from
  source on every run.
- Tighten the **"C-free" wording** in the workflow comments (and CLAUDE.md if touched).
- **Validation gate:** the matrix already builds all five natively; confirm the musl binary
  is `not a dynamic executable` (`ldd`) and `cargo tree -i aws-lc-rs` is empty.
- Cross-family review (release surface — a real look).

### PR 3 — keyless signing + SLSA provenance + SBOM (GitHub-native)
The transparency payoff; all free, no middleman. **Realized 2026-07-13 — details and
the decided layout in "Where we are now" above.**
- `cosign` **keyless** signing (`cosign-installer`) over the archives + checksums.
- **SLSA build provenance** via `actions/attest-build-provenance`; add `id-token: write`
  + `attestations: write`.
- **SBOM** via `syft` (SPDX/CycloneDX), attached to the release.
- **Document verification** in the README — the exact `cosign verify-blob --certificate-identity …
  --certificate-oidc-issuer …` and `gh attestation verify` invocations. Verification an
  operator can't run is theater.

### PR 4 — ghcr distroless image (multiarch, non-root) — a first-class distribution path
Lands *after* signing so the image is **born signed** — we don't ship an unsigned primary
artifact.
- **Multiarch `amd64`+`arm64`** manifest `FROM gcr.io/distroless/static:nonroot`, copying
  the static musl binaries already built per-arch; `USER nonroot`. Push to
  `ghcr.io/tobert/kaibo` (+`latest`); `cosign` signs the image with the PR 3 machinery.
- Workflow gains ghcr login + `packages: write`.
- **User/volume mapping is the core UX to document** (not `-i`): a read-only project mount +
  UID mapping (docker `-u $(id -u):$(id -g)`, podman `--userns=keep-id`), and the
  **devcontainer recipe** — the sweet spot, where mounts are already declared and kaibo
  slots in as an MCP server. There is no truly zero-mount form (kaibo's job is reading your
  code), so the documented baseline mounts cwd read-only; the configurator tailors the rest.
- **Graceful no-stdin error**, not a silent EOF-exit 0 — the entrypoint detects a missing
  stdin and prints a clear message (`distroless` has no shell to debug into). Smoke-test an
  outbound HTTPS call (TLS certs present in distroless).

### Container UX: the configurator / `reconfigure` pass (related host-agent workstream)
The Docker downside is real — a verbose `docker run -i -u … -v …` line is painful to get
right in `claude mcp add`, and iterating means a frustrating remove/re-add loop. Fix it in
two phases: a known-good **baseline** `mcp add` (cwd mounted read-only) connects you, then a
companion **`/reconfigure`** rewrites the *stored* MCP config **in place** for the user's
real setup — podman vs docker, UID mapping, extra roots/worktrees, network policy,
devcontainer nesting — which kills the re-add loop (you edit `~/.claude.json`, you don't
re-run `mcp add`).

**Where it lives matters for the invariants:** kaibo is read-only and runs no external
commands, so it **cannot** write `~/.claude.json` or run docker itself. So `reconfigure` is
a **host-agent skill / a kaibo-provided MCP *prompt***: kaibo *advises* (it already knows
the roots/mounts it needs and exposes them via `kaibo://config`, so it can hand the agent
the exact `-v` flags), and the host agent *acts* (edits the config with its own tools). That
split keeps kaibo's stdio/read-only invariants intact. A tiny generated `kaibo-docker`
wrapper script is an alternative the configurator could emit (point `mcp add` at the wrapper,
iterate the wrapper, not the stored command). This is a distribution-UX workstream that
rides alongside PR 4, not release-pipeline YAML.

### PR 5 — channels (Homebrew first), gated on demand
- **Pre-req (external):** create the `tobert/homebrew-kaibo` tap repo + a token with access.
- A Homebrew formula push (a small action, or a hand-rolled step). **This is the one place
  GoReleaser-OSS earns its keep** — if/when we want brew **and** Scoop **and** Winget **and**
  nfpm, its declarative, correct-by-construction channel blocks beat hand-rolling each. Reach
  for it *here, OSS-only*, not as the spine. Until channels multiply, hand-roll the one tap.

### Deferred decisions (own PRs when the cost is justified)
- **macOS notarization** — Apple Developer account ($99/yr) for the cert; we already build on
  a Mac runner, so the notarization step is scriptable (or a GoReleaser-OSS `notarize` block
  if it's in by then). Until then macOS binaries are unsigned: a Homebrew-tap install is fine,
  a direct download trips Gatekeeper (`xattr -d com.apple.quarantine`).
- **Windows Authenticode** — a code-signing cert ($). Until then the `.exe` is unsigned
  (SmartScreen friction). MSVC-native (our path) is *less* AV-hostile than a MinGW/gnu build
  would have been, but unsigned is still unsigned.

---

## Future: reuse this pipeline elsewhere

The broadly *reusable* part is the **GitHub-native signing + SLSA provenance + SBOM layer**
— it's language-agnostic and bolts onto any tag-triggered workflow. kaibo becomes the
reference for the signing/attestation layer other projects lack:
- **otel-cli (Go)** already runs GoReleaser-OSS; it just gains the `cosign`/attestation/SBOM
  layer.
- **winit / GUI projects (Rust)** — a native-matrix spine generalizes to them fine (native
  Windows runners build MSVC, which the Windows graphics stack — `windows-rs`/wgpu/DirectX —
  needs). The only thing that *wouldn't* have generalized was the abandoned single-runner
  zigbuild-cross path (gnu-only Windows); since we're native-matrix, that concern is moot.

## Open decisions

- **Docker — RESOLVED (2026-06-25, with Amy): keep & promote** to a first-class, early
  distribution path (multiarch, non-root default, devcontainer-friendly). See decision 4 and
  PR 4.
- **Configurator / `reconfigure` ownership & scope** — a kaibo MCP prompt, a Claude Code
  skill, or both? How much does it auto-detect (podman/devcontainer/UID) vs. ask? It edits a
  sensitive file (`~/.claude.json`) with the *host agent's* tools (kaibo can't), so consent/
  safety shape it. Design when PR 4's container UX is real.
- **How many channels?** Few (a brew tap) → hand-roll, no GoReleaser. Many (brew+scoop+winget+nfpm)
  → bring in GoReleaser-OSS as the back-half. Decide when demand is real, not now.
- **macOS signing timeline** — notarization needs the $99/yr Apple account; decide when a
  public release makes Gatekeeper friction worth paying down.
