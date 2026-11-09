# Security Policy

## Supported Versions

Shoal is pre-1.0. Security fixes are applied to the `main` branch and the most
recent tagged release only.

| Version | Supported |
| ------- | --------- |
| `main`  | ✅        |
| latest tagged release | ✅ |
| older releases | ❌ |

## Reporting a Vulnerability

Please **do not** open a public GitHub issue for security vulnerabilities.

Instead, use GitHub's private vulnerability reporting ("Report a vulnerability"
under the repository's Security tab), or email the maintainers at the address
listed in `CODEOWNERS`. Include:

- A description of the issue and the affected component
  (`shoal-server`, `shoal-core`, `shoal-cache`, SDKs, deployment manifests).
- Reproduction steps or a proof of concept if you have one.
- The version / commit hash you tested against.
- Your assessment of impact (data exposure, privilege escalation, DoS, etc.).

We aim to acknowledge reports within **3 business days** and to provide a fix
or documented mitigation within **30 days** for high-severity issues.

## Scope

In scope:

- The HTTP API server (`shoal-server`): authentication, authorization,
  rate limiting, audit logging, request handling.
- The storage engine (`shoal-core`): WAL, segments, manifests, recovery,
  reference counting for branched namespaces.
- The cache hierarchy (`shoal-cache`): disk cache path handling, eviction,
  cross-namespace isolation.
- Official Python and TypeScript SDKs.
- Reference Docker Compose / Kubernetes deployment artifacts shipped in this
  repository.

Out of scope:

- Vulnerabilities in the object storage provider itself (AWS S3, MinIO, etc.).
- Misconfiguration of deployments that deviates from the hardening guide
  (`docs/security/hardening.md`) — though we welcome reports that the guide
  itself is wrong or incomplete.
- Denial of service via volumetric network flooding (handle at the
  infrastructure layer).

## Security Documentation

- [Threat model](docs/security/threat-model.md)
- [Deployment hardening guide](docs/security/hardening.md)
