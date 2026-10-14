# Security Policy

## Supported Versions

Dylos is currently a pre-release technical spike. Only the latest commit on `main` receives security fixes.

| Version | Supported |
| --- | --- |
| `main` | :white_check_mark: |
| < 0.1.0 | :x: |

## Reporting a Vulnerability

If you discover a security vulnerability, report it **only** through [GitHub Private Vulnerability Reporting](https://github.com/lyzstrik/dylos/security/advisories/new).

**Do not open a public issue or discuss the vulnerability publicly.** There is no email reporting address; all reports must go through GitHub private vulnerability reporting.

### What to Include

To help us assess and resolve the issue quickly, include:
- A clear description of the vulnerability.
- The affected component(s) and commit SHA.
- Step-by-step reproduction instructions or a minimal proof of concept.
- The potential security impact and attack scenario.

### Response and Coordinated Disclosure

This project is maintained on a best-effort basis by a single maintainer. The maintainer aims to acknowledge reports within 7 days.

We follow coordinated disclosure: please allow the maintainer reasonable time to investigate and produce a fix before disclosing any details publicly. Once a fix is ready, a security advisory will be published along with the patch.

## Scope and Accepted Risks

Please consult [`docs/security/threat-model.md`](docs/security/threat-model.md) and [ADR-0004](docs/adr/0004-privilege-model.md) before reporting. Known risks accepted for the spike are documented there and are **not** considered vulnerabilities:

- **Root orchestrator**: The orchestrator runs as root during the technical spike to launch the upstream jailer (ADR-0004). Privilege separation is planned post-spike.
- **Shared kernel inode owned by the jailer uid**: Shared kernel files hard-linked into microVM jails are owned by the shared jailer uid during the spike (`docs/security/threat-model.md`, LYZ-47).
- **Unfiltered traffic between peers on a segment**: Network bridges isolate labs from each other and the host, but segment peer filtering (NDP/ARP spoofing prevention) is not implemented during the spike (`docs/security/threat-model.md`, LYZ-48).
