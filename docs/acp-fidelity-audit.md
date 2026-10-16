# ACP fidelity audit

Status: in progress. This document records an audit of the browser protocol
against the pinned ACP schema. Runtime code is not migrated by this change.

## Baseline

| Component | Pinned version | Source |
|-----------|----------------|--------|
| Rust SDK `agent-client-protocol` | 2.0.0, git rev `754d5aa1` | `Cargo.toml`, `Cargo.lock` |
| Rust schema `agent-client-protocol-schema` | `=1.7.0`, module `v1`, SDK feature `unstable` | `Cargo.lock` |
| TypeScript SDK `@agentclientprotocol/sdk` | 1.4.0 | `package-lock.json` |

Upstream drafts and newer releases are listed separately and are not claimed
as supported.
