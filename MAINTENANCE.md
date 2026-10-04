# Maintenance policy

## Status

Pensyve Cloud, the hosted service, closed on 2026-10-01. The dashboard at
pensyve.com, the API at api.pensyve.com, and the MCP endpoint at
mcp.pensyve.com no longer exist.

Pensyve continues as an Apache-2.0 project that you run yourself. See
[`docs/self-host.md`](docs/self-host.md) for how to deploy it.

The project is in maintenance mode.

## What gets released

- Security fixes.
- Dependency updates.

## What does not get released

- New features.

## Issues and pull requests

Issues and pull requests are welcome. They are handled on a best-effort basis,
and there is no guaranteed response time. A pull request that adds a feature
may be declined even if it is well made. Forks are welcome under the terms of
the [license](LICENSE).

## Historical research material

Earlier release and research tools remain in the repository for reference.
For example, `scripts/v2_1_release_gate.sh` and
`pensyve-python/tests/test_footprint.py` refer to artifacts from the separate
`pensyve-docs` repository. Those tools are outside the current build and
`make check` workflow. Use [CONTRIBUTING.md](CONTRIBUTING.md) for current
development instructions.

## Reporting a security issue

Do not open a public issue. Follow [`SECURITY.md`](SECURITY.md): report the
issue through
[GitHub's private vulnerability reporting](https://github.com/major7apps/pensyve/security/advisories/new).
