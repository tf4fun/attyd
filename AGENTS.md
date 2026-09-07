# Repository Guidelines

## Project Structure & Module Organization

`attyd` exposes an Agent Client Protocol (ACP) agent through a web workspace.

- `src/`: Rust binary, ACP bridge, runtime state, filesystem, and terminal services; entry point: `main.rs`.
- `web/src/`: React UI, ACP components, browser state/helpers, and `styles.css`; `web/index.html` is the Vite entry.
- `shared/`: TypeScript browser protocol types and validation helpers.
- `tests/`: frontend tests, `browser/` Playwright cases, and reusable `fixtures/`.
- `scripts/`: integration, smoke, and coverage drivers; `docs/`: architecture and acceptance ledgers.
- `dist/client/`: generated assets embedded into the Rust executable; keep build outputs untracked.

## Build, Test, and Development Commands

Use Rust 1.88+ and Node.js 20+; CI uses Node.js 22. Run `npm ci` to install dependencies.

- `npm run dev -- -- your-agent acp`: run the Rust host with an ACP agent at `http://127.0.0.1:7331`.
- `npm run build`: build the web bundle and release executable; `npm start` runs it.
- `npm run check`: typecheck TypeScript, run Vitest, and build the client.
- `npm run test:rust`: run native Rust tests.
- `npm run test:remote` / `npm run test:ui`: check remote transports and the REST/SSE bridge.
- `npm run test:browser`: build and run Playwright; local runs require installed Chrome.

## Coding Style & Naming Conventions

Follow existing formatting: Rust uses four-space indentation and snake_case modules/functions; TypeScript/TSX uses two spaces, double quotes, and semicolons. Use PascalCase React components and types, camelCase functions, and kebab-case frontend filenames such as `prompt-composer.tsx`. Keep TypeScript strict. Use `cargo fmt` for Rust; no dedicated JavaScript formatter or linter is configured.

## Testing Guidelines

Use Vitest (`tests/*.test.ts`, `*.test.tsx`), Playwright (`tests/browser/*.pw.ts`), and colocated Rust `#[cfg(test)]` modules. Add behavior-focused regression tests at the affected layer. Follow `docs/bridge-state-machine-tdd.md` for lifecycle/concurrency changes; verify bridge behavior before browser projections.

`npm run test:coverage` requires `cargo-llvm-cov` and enforces 85% Rust line coverage by default. See `docs/testing.md` for additional suites. Run relevant integration checks alongside `npm run check` and Rust tests.

## Commit & Pull Request Guidelines

Follow the history's concise, imperative Conventional Commit style: `fix: preserve turn outcome boundaries`; observed prefixes include `feat:`, `fix:`, `refactor:`, and `ci:`. Describe behavior changes and validation commands/results in PRs. Link relevant issues and include screenshots for visible UI changes.
