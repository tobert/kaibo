# Releasing kaibo — pipeline plan & living record

解剖（かいぼう）ships as a single static-ish binary per platform. This document is the
*living* plan for how we build and publish those binaries — the sequence of PRs that
gets us from "a workflow that has never fired" to "signed, attested, multi-channel
releases an agent operator can trust." Update it as PRs land; it is the durable map the
conversation can't reconstruct.

> **Gating note.** This whole pipeline is **parked behind the pre-1.0 work** — kaibo is
> not ready for a public release yet, and we will not cut `v*` tags in anger until that
> lands. The value of writing the plan now is to *settle the decisions* so we don't
> re-litigate them later, and to let the PRs land incrementally (each is independently
> reviewable and mergeable) as the runway clears. Nothing here forces a release.

---

## Where we are now

`.github/workflows/release.yml` already exists: a hand-rolled 6-target matrix (musl
static via `cargo-zigbuild`, native macOS/Windows), `tar.gz`/`zip` packaging, sha256
sidecars, and a `softprops/action-gh-release` publish on a `v*` tag. **It has never
fired.** It is a good baseline — the static-musl + ring/no-aws-lc groundwork is real —
but it does no Docker, no signing, no package channels, and it pins its actions by
floating tag.

## The decisions (2026-06-25, with Amy)

1. **Adopt GoReleaser.** Replace the hand-rolled matrix with a `.goreleaser.yml` driven
   by `goreleaser-action`. Amy has run GoReleaser on `otel-cli` (Go) for years, so the
   `.goreleaser` shape, ghcr `dockers`/`docker_manifests`, and `brews` tap pattern are
   familiar — kaibo reuses those patterns rather than inventing them. One config
   collapses build + archive + checksum + changelog + release, and unlocks
   Homebrew/Scoop/Winget, Docker, and signing as additive blocks.
2. **Container images on `ghcr.io`, distroless base.** Same OIDC/`GITHUB_TOKEN` trust
   path as the repo, no Docker Hub account or pull-rate tax. Base
   `gcr.io/distroless/static` (CA certs / tzdata / nonroot user available if the stdio
   process ever needs them), multi-arch `amd64`+`arm64` via `docker_manifests` — mirrors
   the `otel-cli` ghcr layout.
3. **Sigstore keyless signing + SLSA provenance.** `cosign` keyless (OIDC, *no
   long-lived key to manage or leak*) over the archives, checksums, and image, plus
   GitHub build-provenance attestation. This is new ground — `otel-cli` has no `signs:`
   block — and it is the concrete payoff of staying on public CI (see "Why public CI").

## Why GoReleaser, and the one thing Go never made us face

GoReleaser's **Rust builder** (`builder: rust`, since v2.5) defaults to `cargo-zigbuild`
— the *same* toolchain the current workflow already uses for musl. So the build engine
doesn't change; GoReleaser just orchestrates it and everything downstream.

The gotcha: **`cargo-zigbuild` can target `windows-gnu` but not `windows-msvc`** (zig
links its bundled MinGW-w64, not MSVC). `otel-cli` cross-compiles MSVC-free Windows
trivially *because it's Go*; Rust+zig cannot. GoReleaser's OSS path builds every target
on **one Linux runner** (multi-runner "split & merge" is a **Pro** feature), so the
coherent OSS choice is to cross-compile all targets from Linux:

| Target | How | Notes |
| --- | --- | --- |
| `x86_64-unknown-linux-musl` | zigbuild | fully static (`not a dynamic executable`) |
| `aarch64-unknown-linux-musl` | zigbuild | fully static |
| `x86_64-apple-darwin` | zigbuild cross | builds unsigned; notarization deferred |
| `aarch64-apple-darwin` | zigbuild cross | builds unsigned; notarization deferred |
| `x86_64-pc-windows-gnu` | zigbuild cross | **was `-msvc`** — see invariant change below |

**This cross-compile-from-one-runner is only feasible because of the TLS invariant**
(rustls + ring, no aws-lc-sys / no OpenSSL): a C-free tree is what lets zig link
darwin/windows-gnu without an MSVC install or a fragile aws-lc cross-build. The
invariant we already hold is what makes the cheap path possible.

### Invariant change this forces (do not skip)

The CLAUDE.md **Build & release** section currently says *"Windows statically links the
CRT via `+crt-static`"* (MSVC). Moving to `cargo-zigbuild` makes that **`x86_64-pc-
windows-gnu`, statically linked against zig's bundled MinGW-w64.** The PR that lands the
GoReleaser build (PR 2 below) must reword that bullet and the `.cargo/config.toml`
note. For a CLI/MCP binary the gnu ABI is a non-issue; we trade the MSVC ABI for a
single-runner OSS build. *(Open question — see below — is whether keeping MSVC + native
runners is worth GoReleaser Pro. Default answer: no.)*

## The deferred paid toll (no tool removes these)

- **macOS notarization** — needs a Mac + an Apple Developer account ($99/yr). Until
  then macOS binaries are *unsigned*: a Homebrew-tap install is fine (not quarantined),
  but a direct download trips Gatekeeper (`xattr -d com.apple.quarantine`). Document
  honestly; revisit when a public release justifies the cost.
- **Windows Authenticode** — needs a code-signing cert ($). Until then the `.exe` is
  unsigned (SmartScreen friction). Same posture: document, defer.

Both are *automatable* by GoReleaser when we choose to pay; they're a cost decision, not
a tooling gap.

## Why public CI (and not local builds)

Amy's instinct was that *no* public CI keeps kaibo off the radar of botnets hunting CI
to attack, and that local builds might feel more honest. The analysis landed the other
way, and it's worth recording so we don't reopen it:

- The mass-attack vectors hunt for workflows that run on **fork PRs** (free-runner
  crypto-mining) or that leak **long-lived secrets**. A **tag-only + `workflow_dispatch`**
  release workflow with **no PAT** dodges both by shape.
- The real residual risk is a **compromised third-party action** (cf. the
  `tj-actions/changed-files` incident). Mitigation is cheap: **pin every `uses:` to a
  commit SHA**, which every PR below does.
- **Local builds fight the transparency goal.** A public CI log is a third-party-
  witnessed, reproducible record; a laptop is an opaque box whose provenance is "Amy
  says so." The modern answer to "why trust this binary" is **keyless provenance
  attestation**, which is only cheaply attainable from public CI with OIDC — local
  builds *structurally cannot* produce it. So public CI, hardened, maximizes
  transparency **and** minimizes realistic attack surface; going dark trades the goal
  for obscurity.
- If hardware control is ever the itch, a **self-hosted runner** scratches it without
  losing the public workflow — but for a tag-only release the GitHub-hosted runners
  cost nothing and carry less surface. Not planned.

---

## The PR sequence

Each PR lands on its own branch in a **git worktree** (Amy's workflow for this series),
goes up as a PR, gets a **cross-family review** (a different model lineage than wrote
it), and updates `CHANGELOG.md` where user-facing. They're ordered lowest-coupling
first; signing comes after the artifacts exist; channels (which need external repos)
come last.

> **Pre-flight (not a PR):** fire the *existing* `release.yml` once via
> `workflow_dispatch` to confirm the current matrix still produces good binaries — a
> known-good baseline before GoReleaser's complexity. If it's already untrusted, skip
> straight to PR 2 and validate there with `goreleaser release --snapshot --clean`.

### PR 1 — this document
The living plan itself (`docs/releases.md`). Settles the decisions above. No pipeline
change. *(You are reading the artifact.)*

### PR 2 — GoReleaser core: build + archive + checksum + release
The foundation everything hangs off.
- Add `.goreleaser.yml` (`version: 2`, `builder: rust`, the 5 targets above), `archives`
  (`tar.gz`; `zip` override for windows), `checksum`, `changelog`.
- Replace the hand-rolled matrix in `release.yml` with a thin `goreleaser-action`
  workflow — **SHA-pinned** actions, keep `tags: ["v*"]` + `workflow_dispatch`, minimal
  `permissions`. Install rust + zig + `cargo-zigbuild` in the job (GoReleaser does *not*
  install them; it does run `rustup target add`).
- **Reword the CLAUDE.md Windows invariant** (`-msvc` → `-gnu`) and the `.cargo/config.toml`
  note.
- **Validation gate:** `goreleaser release --snapshot --clean` builds all 5 targets
  locally; confirm the musl binary is `not a dynamic executable` (`ldd`), the macOS
  cross-link succeeds (ring-only/no-aws-lc is what lets it), and the windows-gnu `.exe`
  is produced. `cargo tree -i aws-lc-rs` stays empty.
- Cross-family review (build/release surface — a real look, not a glance).

### PR 3 — ghcr distroless multi-arch image
- `dockers:` (amd64 + arm64) + `docker_manifests:` → `ghcr.io/tobert/kaibo:<tag>` and
  `:latest`, mirroring the `otel-cli` layout. `release/Dockerfile` `FROM
  gcr.io/distroless/static` copying the static musl binary.
- Workflow gains ghcr login + `packages: write`.
- Document the **stdio caveat** in the image usage (`docker run -i ...`): the image is
  for a pinned, pullable env, not a long-running service (kaibo never binds a socket).

### PR 4 — Sigstore keyless signing + SLSA provenance
The transparency payoff; new ground vs. otel-cli.
- `signs:` (cosign keyless, `cosign-installer`) over checksums + artifacts; `docker_signs:`
  for the image.
- GitHub build-provenance attestation (`actions/attest-build-provenance`); add
  `id-token: write` + `attestations: write`.
- Document verification (`cosign verify-blob` / `gh attestation verify`) in the README so
  an operator can actually check it — signing nobody verifies is theater.

### PR 5 — Homebrew tap
- `brews:` → `tobert/homebrew-kaibo` tap (mirror otel-cli: `url_template`, `repository`,
  `skip_upload: "auto"` so prerelease tags don't publish). Requires creating the tap repo
  and a token with access.
- *(Optional, fold-or-defer:)* `nfpms:` (deb/rpm/apk) as otel-cli does — kaibo is niche,
  so gate on whether anyone wants distro packages.

### PR 6 — Windows channels: Scoop + Winget
Additive, lowest priority (smallest audience). Winget also needs an external PR to
`microsoft/winget-pkgs`. Land only if Windows demand is real.

### Deferred decisions (own PRs when the cost is justified)
- **macOS notarization** — Apple Developer account + a Mac runner (or GoReleaser Pro
  split to a native macOS runner). Cost decision.
- **Windows Authenticode** — code-signing cert. Cost decision.

---

## Future: reuse this pipeline elsewhere

If this pipeline works out, Amy wants to bring the modern bits to other projects. The
broadly *reusable* parts are the **signing + provenance attestation, distroless base,
and ghcr hardening** patterns — kaibo becomes the reference for the signing/attestation
layer those projects don't have yet. The **build** layer does *not* generalize, and the
reason is exactly the `windows-gnu` constraint kaibo accepts:

- **otel-cli (Go).** Go cross-compiles native Windows MSVC-free, so it stays on the
  Pro-free native-cross path and keeps its existing nfpms/brew/docker blocks. It just
  gains the signing/attestation layer.
- **winit / GUI projects (Rust).** These are the case kaibo's cheap path *can't* be
  copied onto. winit drags in the Windows desktop/graphics stack (`windows-rs`, wgpu,
  DirectX), which is **MSVC-centric** — `windows-gnu` is nominally a target but the
  ecosystem assumes msvc, so `cargo-zigbuild`'s gnu-only Windows build won't serve them.
  That is precisely where **GoReleaser Pro split-and-merge** (or a dedicated native
  `windows-msvc` runner job) earns its cost. Don't copy kaibo's one-runner gnu build
  onto a winit app and expect it to link.

## Open decisions

- **Windows ABI — RESOLVED for kaibo (2026-06-25, with Amy): `windows-gnu`.** One OSS
  runner, no Pro, no MSVC. Fine for a CLI/MCP binary, and Pro's cost would only buy the
  MSVC ABI + native notarization we're deferring anyway. *This resolution is
  kaibo-specific* — winit/GUI Rust projects need MSVC (see "reuse this pipeline
  elsewhere"), so the decision does not carry to them.
- **Drop a platform?** kaibo's audience is agent-operator dev machines — Linux + macOS
  are the weight; Windows is nonzero but small. We keep all five for now; if windows-gnu
  cross-compile proves fragile, dropping Windows (or moving it to nfpms/brew only) is on
  the table.
- **nfpms in PR 5 or skip?** Gate on real demand for distro packages.
