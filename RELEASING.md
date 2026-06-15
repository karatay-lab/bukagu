# Releasing bukagu

Distribution is automated by [`.github/workflows/release.yml`](.github/workflows/release.yml),
triggered by pushing a semver tag. Each release publishes:

| Channel            | Artifact                                              | Always runs? |
| ------------------ | ---------------------------------------------------- | ------------ |
| Static binaries    | `bukagu-vX.Y.Z-<arch>-unknown-linux-musl.tar.gz` (+ `.sha256`) | yes |
| Debian/Ubuntu      | `bukagu-vX.Y.Z-<amd64\|arm64>.deb`                    | yes |
| crates.io          | `cargo install bukagu`                                | only if `CARGO_REGISTRY_TOKEN` is set |
| Homebrew tap       | `karatay-lab/homebrew-tap` formula                   | only if `HOMEBREW_TAP_TOKEN` is set |

## Cutting a release

```bash
# 1. Bump the version in Cargo.toml, commit.
# 2. Tag and push:
git tag v0.1.1
git push origin v0.1.1
```

The workflow creates the GitHub Release and attaches everything above.

## One-time setup

### crates.io
1. Claim the name with a manual first publish (the workflow can't claim it for you):
   `cargo login` then `cargo publish`.
2. Create an API token at <https://crates.io/settings/tokens> and add it as the
   repo secret **`CARGO_REGISTRY_TOKEN`**. After that, every tagged release publishes
   automatically (it will no-op/fail harmlessly if the version already exists).

### Homebrew tap
1. Create a public repo **`karatay-lab/homebrew-tap`**.
2. Create a fine-grained PAT with `contents: write` on that repo and add it here as
   the secret **`HOMEBREW_TAP_TOKEN`**.
3. On each release the workflow renders
   [`packaging/homebrew/bukagu.rb.tmpl`](packaging/homebrew/bukagu.rb.tmpl) (filling in
   version + tarball checksums) and commits it to `Formula/bukagu.rb` in the tap.

Users then install with `brew install karatay-lab/tap/bukagu`. The formula is
Linux-only (the release ships Linux builds); add macOS targets to the workflow and
template if you want `brew` on macOS too.

## How the binaries are built

- **Tarballs** are `*-musl` (fully static, zero runtime deps) via
  `taiki-e/upload-rust-binary-action`, which cross-compiles aarch64 with `cross`.
- **`.deb`s** are `*-gnu` (dynamically linked to the system glibc, the way apt users
  expect) built with `cross` for an old, widely-compatible glibc, then packaged by
  `cargo-deb` using the `[package.metadata.deb]` block in `Cargo.toml`.
