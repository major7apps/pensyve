# Changelog — Pensyve Antigravity CLI Plugin

All notable changes to the native Pensyve Antigravity plugin are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this package follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.0.0] - 2026-08-21

### Added

- Native Antigravity `plugin.json` and URL-only OAuth MCP configuration.
- Eight split working-memory rules within Antigravity's 12,000-character rule limit.
- `/remember`, `/recall`, `/forget`, and `/inspect` workflows as native skills.
- Context loading, refactor briefing, memory review, and session-memory skills.
- Static MCP-contract, provenance, legacy-reference, and package-size validation.

### Design

- Browser OAuth is the default interactive cloud authentication path.
- Local stdio uses the distinct `pensyve-local` server name.
- No lifecycle hooks are added; episodes retain the lazy-open behavior.
