# Repository Guidelines

These conventions apply to every contributor, human or AI agent, working on
zpay. Claude Code specifics live in [CLAUDE.md](CLAUDE.md); everything in this
file applies to every agent.

## Read first, every session

Before suggesting any change, read at least:

1. [docs/architecture/public-interfaces.md](docs/architecture/public-interfaces.md):
   the vocabulary spine. If your change names a new type, error code, config
   field, capability string, or wire message, confirm the name fits the rules
   here.
2. [docs/architecture/operational-surfaces.md](docs/architecture/operational-surfaces.md):
   readiness, ops port, env-var schema, live-test gates.
3. [docs/architecture/facilitator-plane.md](docs/architecture/facilitator-plane.md):
   prepare, settle, watch, verify lifecycle. If your change crosses a plane,
   confirm you are not bypassing the typed contract.
4. The ADR index in [docs/README.md](docs/README.md). If your change
   contradicts an ADR, write a superseding ADR first.

## Documentation is part of the change

When you touch a public boundary, a config field, a vocabulary term, an error
code, or a plane contract, update the relevant architecture doc, ADR, or
`public-interfaces.md` in the same PR. A code change that ships ahead of its
doc change is incomplete.

A new boundary (new wire adapter, new RPC method, new storage table, new
upstream binding) gets an ADR before code lands.

## Forbidden names

The following identifier roots are banned across Rust, TypeScript, SQL, proto,
and config, anywhere in any identifier:

`bar`, `common`, `data`, `foo`, `handler`, `helpers`, `info`, `item`,
`manager`, `obj`, `payload`, `processor`, `result`, `shared`, `stuff`,
`thing`, `tmp`, `utils`, `value`.

As suffixes: `*Api`, `*Data`, `*Helper`, `*Info`, `*Manager`, `*Processor`,
`*Server`, `*Service`, `*Util`.

If a module or symbol cannot be named by domain, the boundary is not
understood. Stop and ask before inventing a meta-name.

## Required suffixes

- Time fields: `_ms`, `_seconds`, `_minutes`, `_hours`, `_blocks`, `_height`.
  Never bare `timeout`, `delay`, `interval`, `expires`.
- Money fields: `_zat` for integer zatoshis, `_zec` for human-readable
  decimal strings. Never bare `amount`.
- Booleans: `is_*`, `has_*`, `can_*`. Never bare `enabled`; use
  `mpp_enabled` or similar. Never negated names like `is_not_ready`.
- Counts: `_count`.
- Bytes: `_bytes`.

## Verbs from the project vocabulary

Domain operations use the project verb set: `accept`, `advertise`, `broadcast`,
`compute`, `confirm`, `derive`, `discover`, `find`, `get`, `observe`, `parse`,
`prepare`, `prove`, `settle`, `sign`, `submit`, `verify`, `watch`.

Generic verbs (`do`, `execute`, `handle`, `manage`, `perform`, `process`) are
forbidden for domain operations. They are acceptable for clearly mechanical
glue (e.g., `handle_signal`).

## Network awareness is non-negotiable

Every domain type, every database row, every API response, every log line
carries a `Network` value (`Mainnet`, `Testnet`, `Regtest`). The flip from
testnet to mainnet must be a configuration change, never a code change. If
you write a function that takes an address but not a network, stop and add
the network parameter. Network mismatches must fail closed at construction
time, not at use time.

## No temporal or implementation drift in names

`new_x`, `x2`, `legacy_x`, `x_old`, `x_final`, `x_real`, `redis_x`,
`libsql_x`, `axum_x` are all banned. The name of a thing must survive a
change of its implementation.

## Error vocabulary

`thiserror` v2 throughout. Each public boundary returns a typed enum; no
`Box<dyn Error>`, no `anyhow`, no `eyre`, no `Other(String)` catch-alls.
Each error variant has a documented retry posture (`retryable`,
`not_retryable`, `requires_operator`) in its rustdoc. Full registry in
[docs/reference/error-vocabulary.md](docs/reference/error-vocabulary.md).

At wire boundaries, typed errors map to HTTP status codes through a single
`into_status()` impl in `zpay-runtime`. Adapters never expose
`reqwest::Error`, `tonic::Status`, or `libsql::Error` upward.

## Secrets

zpay-owned Rust processes receive secrets through canonical typed config
fields populated by `ZPAY_*` variables or the operator-provided TOML layer.
Do not add paired `_path` variants for wallet age identities, bearer-key
allowlist hashes, JWKS pinning material, Turso auth tokens, or any other
secret. Local scripts may read generated files for upstream tools, but they
must pass zpay secrets through the same typed fields Railway uses. Logs,
diagnostics, proof artifacts, and `--print-config` must redact every secret
field; the redactor emits `[REDACTED]` and never the raw value.

## Testing

Tier organisation:

| Tier | Location | Nextest profile |
|------|----------|----------------|
| T0 unit | `#[cfg(test)] mod tests` inside `src/` | default |
| T1 integration | `tests/integration/` per crate | default |
| T2 perf | `tests/perf/` per crate | `ci-perf` (added with first benchmark) |
| T3 live | `tests/live/` per crate | `ci-live` |

Each crate's `tests/acceptance.rs` aggregates the tier submodules via
`mod integration;` (and `mod live;` etc.).

T3 tests are double-gated by `#[ignore = LIVE_TEST_IGNORE_REASON]` and a
runtime `require_live()` call from `zpay-testkit`. Mainnet is rejected by
default; opt in with `ZPAY_TEST_ALLOW_MAINNET=1`. The single test-gate
environment variable is `ZPAY_TEST_LIVE=1`; there is no separate
`ZPAY_TEST_*` namespace for configuration.

Test function names are plain `snake_case_describing_behavior`. Do not
include `live`, `regtest`, `testnet`, or `mainnet` in the function name; the
directory and runtime parameterisation carry that.

## Commits

- Concise imperative subject with an optional scope: `core: refuse
  out-of-range expiry_height` or `x402: settle requires DPoP proof`.
- Conventional-commit prefixes (`feat:`, `fix:`, `docs:`, `chore:`,
  `refactor:`, `perf:`, `test:`) are accepted but not required.
- Do not add `Co-Authored-By:` trailers naming the AI model. The git
  history records the human owner; AI assistance is disclosed in the PR
  description, not the commit metadata.
- No "Generated with Claude Code" footers.
- No em dashes anywhere (code, docs, comments, commits, PR descriptions);
  use colons, semicolons, parentheses, or restructure.
- Pre-commit hooks must pass; never bypass with `--no-verify`.

## Pull requests

- Reference an open issue acknowledged by a maintainer.
- Lead with user-facing impact, not implementation. The first sentence
  should tell a reader who has not opened the codebase what was broken
  or what changed.
- Do not list changed files in the PR body; the GitHub UI shows them.
- Do not add a test plan section unless the issue requires one.
- Disclose AI tool usage in the PR description, not in the commit metadata.
- Keep PRs focused on one logical change. A new wire adapter, a new
  storage migration, and a refactor of `zpay-core` types are three PRs.

## Reviews

- Use `gh api` to create reviews as `pending`; never submit reviews directly.
- Inline comments read as one engineer talking to another: factual,
  direct, conversational.
- Never push commits, publish reviews, or submit anything without explicit
  confirmation from the human owner.
- `gh pr review --approve`, `--request-changes`, and `--comment` are
  forbidden; they submit immediately.

## Style

- No em dashes. Use colons, semicolons, parentheses, or restructure the
  sentence.
- No emojis in source, docs, commits, or PR bodies unless explicitly
  requested by the human owner.
- Default to writing no code comments. Add a comment only when the why
  is non-obvious. Never narrate prior approaches; git history holds that.
- Do not write multi-paragraph rustdoc summaries for internal items.
  Public items document the contract; private items document only what
  surprises a reader.

## Ecosystem position

zpay is the payments-protocol peer to
[zally](https://github.com/gustavovalverde/zally) (Rust wallet library)
and [zinder](https://github.com/gustavovalverde/zinder) (Zcash indexer).
Both upstreams are consumed by pinned git rev; bumps land as their own PRs
naming the upstream change in the body.

## When in doubt

Open an issue and ask before touching the spine. Vocabulary breaks are
expensive to revert.
