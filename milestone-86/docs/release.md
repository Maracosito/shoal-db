# Release process

Shoal is released as a coordinated set of artifacts from a single git tag.
Pushing a tag of the form `vX.Y.Z` triggers `.github/workflows/release.yml`,
which produces:

| Artifact | Destination |
| --- | --- |
| CLI binaries (Linux x86_64, macOS arm64/x86_64) | GitHub Release assets (`.tar.gz` + `.sha256`) |
| Python SDK (sdist + wheel) | PyPI |
| TypeScript SDK | npm |
| Server Docker image | `ghcr.io/<org>/<repo>:X.Y.Z`, `:X.Y`, `:latest` |
| Helm chart | `oci://ghcr.io/<org>/charts/shoal` + GitHub Release asset |

## One-time repository setup

1. **PyPI** — configure a [Trusted Publisher](https://docs.pypi.org/trusted-publishers/)
   for this repository pointing at the `release.yml` workflow and the
   `release` environment. No API token is stored in the repository.
2. **npm** — create an automation token with publish rights for the SDK
   package and store it as the `NPM_TOKEN` repository secret.
3. **GHCR** — no setup required; the workflow authenticates with the
   built-in `GITHUB_TOKEN`. Make the published packages public from the
   GitHub Packages UI after the first release if desired.
4. Create a GitHub Actions **environment** named `release` and (recommended)
   add required reviewers to it, so publishing to public registries needs an
   explicit approval.

## Cutting a release

1. Pick the version `X.Y.Z` (semver; breaking API changes bump the major
   once we leave 0.x, per the roadmap).
2. Bump versions in one commit:
   - `cli/Cargo.toml` — `version`
   - `sdks/python/pyproject.toml` and `sdks/python/shoal/_version.py`
   - `sdks/typescript/package.json`
   - `deploy/helm/shoal/Chart.yaml` — `version` and `appVersion`
     (the release workflow also overrides these at package time from the tag,
     so the committed values are documentation more than mechanism)
3. Open a PR, let CI (`ci.yml`) and the e2e suite (`e2e.yml`) go green, merge.
4. Tag and push:

   ```bash
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```

5. Approve the `release` environment when prompted. The workflow creates a
   draft-free GitHub Release with auto-generated notes and all binary assets
   attached.

## Verifying a release

```bash
# CLI
curl -LO https://github.com/<org>/<repo>/releases/download/vX.Y.Z/<bin>-X.Y.Z-x86_64-unknown-linux-gnu.tar.gz
shasum -a 256 -c <bin>-X.Y.Z-x86_64-unknown-linux-gnu.tar.gz.sha256

# SDKs
pip install shoal==X.Y.Z
npm view <package> version

# Image + chart
docker pull ghcr.io/<org>/<repo>:X.Y.Z
helm pull oci://ghcr.io/<org>/charts/shoal --version X.Y.Z
```

## Hotfixes

Branch from the release tag, cherry-pick the fix, bump the patch version,
and tag from the hotfix branch. The pipeline is branch-agnostic — it only
keys off tags — so no special handling is needed.

## Yanking a bad release

- **PyPI**: `Manage project → release → yank` (keeps installs reproducible
  for pinned users while hiding it from resolution).
- **npm**: `npm deprecate <pkg>@X.Y.Z "reason"` — prefer deprecation over
  `npm unpublish`.
- **GHCR**: delete the version from the Packages UI; `:latest` should be
  re-pointed by cutting a new patch release rather than re-tagging.
