<!--
Rendered onto every release page by release.yml, prepended above GitHub's
auto-generated notes. envsubst fills TAG (the git tag, v-prefixed) and VERSION
(the same without the v — how the image is tagged). This is the one surface
where every placeholder is concrete — the commands below carry the exact tag,
ready to copy — so keep it that way: no angle-bracket stand-ins. HTML comments
don't render on GitHub, so this block ships invisibly (envsubst reaches inside
it, which is why it names the variables in prose).
-->
## Get it

Grab your platform's archive below ([install + MCP registration](https://github.com/tobert/kaibo#installation)) — or take the [container image](https://github.com/tobert/kaibo/pkgs/container/kaibo):

```sh
docker pull ghcr.io/tobert/kaibo:${VERSION}
```

```dockerfile
# one-line install into a devcontainer or any image — the binary is fully static
COPY --from=ghcr.io/tobert/kaibo:${VERSION} /usr/local/bin/kaibo /usr/local/bin/kaibo
```

(Pull version tags like `${VERSION}` — the `sha256-*` tags on the package page are
cosign signature/attestation artifacts riding alongside the image, not images.)

## Verify it

Run these from the folder holding your downloads. Every artifact carries SLSA
build provenance — any downloaded file, one command:

```sh
gh attestation verify kaibo-${TAG}-x86_64-unknown-linux-musl.tar.gz -R tobert/kaibo
gh attestation verify oci://ghcr.io/tobert/kaibo:${VERSION} -R tobert/kaibo
```

Or keyless-verify the signed checksum manifest with cosign ≥ 3 (covers every file it lists, works offline):

```sh
cosign verify-blob \
  --bundle checksums.txt.sigstore.json \
  --certificate-identity "https://github.com/tobert/kaibo/.github/workflows/release.yml@refs/tags/${TAG}" \
  --certificate-oidc-issuer "https://token.actions.githubusercontent.com" \
  checksums.txt
sha256sum -c --ignore-missing checksums.txt
```

---
