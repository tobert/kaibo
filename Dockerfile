# kaibo container image — the fully-static musl binary in a distroless, shell-less base.
#
# Because the binary links against nothing (see AGENTS.md "Build & release"), this
# image doubles as a COPY source: pull kaibo into any other image — a devcontainer
# especially — with one line, no libc or package concerns, FROM scratch to Ubuntu:
#
#   COPY --from=ghcr.io/tobert/kaibo:latest /usr/local/bin/kaibo /usr/local/bin/kaibo
#
# Built multiarch (amd64 + arm64) by .github/workflows/release.yml from the per-arch
# musl release binaries staged under binaries/<arch>/ — pure COPY, no RUN, so neither
# platform needs emulation to build.
#
# distroless/static:nonroot: no shell, no package manager, runs as nonroot (uid
# 65532), and ships CA certificates at /etc/ssl/certs — which rustls-native-certs
# finds for kaibo's outbound provider calls. Pinned by manifest-list digest, same
# discipline as the workflow's action pins.
FROM gcr.io/distroless/static:nonroot@sha256:d29e660cc75a5b6b1334e03c5c81ccf9bc0884a002c6000dbf0fb96034814478

# Links the ghcr package to the repo (README, visibility inheritance) even for a
# manual build; the workflow's metadata-action adds the full OCI label set on top.
LABEL org.opencontainers.image.source="https://github.com/tobert/kaibo" \
      org.opencontainers.image.description="grounded, cited, read-only codebase consultation from a model outside your agent's family — stdio MCP server" \
      org.opencontainers.image.licenses="MIT"

ARG TARGETARCH
COPY binaries/${TARGETARCH}/kaibo /usr/local/bin/kaibo

# /work is the canonical project mount: kaibo scopes its read-only access to the
# cwd when --root isn't given, so `-v "$PWD:/work:ro"` is the whole setup. The
# container runs kaibo as an MCP server over stdio — `-i` is load-bearing (without
# it stdin is closed immediately and the server exits at once, silently).
WORKDIR /work
ENTRYPOINT ["/usr/local/bin/kaibo"]
