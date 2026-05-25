# Claude Code Conventions

Claude Code specifics for zpay. Read [AGENTS.md](AGENTS.md) first; this file
only adds Claude-specific guidance.

## Working directories

zpay depends on three sibling repositories that live next to it on disk. Add
them to your session if you need to read them:

- `/Users/gustavovalverde/dev/zfnd/zally`: Rust wallet library. zpay-core
  depends on zally-core, zally-keys, zally-chain, and zally-wallet.
- `/Users/gustavovalverde/dev/zfnd/zinder`: next-generation Zcash indexer.
  zpay calls `BroadcastTransaction` and subscribes to `ChainEvents` and
  `MempoolEvents` via `zinder-client::RemoteChainIndex`.
- `/Users/gustavovalverde/dev/zfnd/fauzec`: testnet faucet. zpay borrows
  fauzec's libSQL connection wrapper, env-var schema, and ops-port shape.

Read-only references that inform decisions but should not be modified from
this repository:

- `/Users/gustavovalverde/dev/zfnd/zexplorer`: chain-read BFF. Provides the
  optional fallback confirmation oracle and the per-txid watch endpoint zpay
  consumes when not subscribed directly to zinder.
- `/Users/gustavovalverde/dev/personal/zentity`: identity platform. Issues
  the PoH SD-JWT-VC tokens zpay validates as part of the x402 compliance
  extension.

## Reading the spine before edits

When asked to change anything in `crates/`, `proto/` (once it exists),
`migrations/`, or `docs/architecture/`, read the relevant architecture doc
first. The spine is short on purpose; you can read the whole thing in a few
minutes. Skipping it produces drift that costs more to revert later.

## Skills that match common zpay work

Use these skills when they match the user's request, instead of redoing the
work by hand:

- `/review`: review a zpay PR.
- `/security-review`: pre-merge security review on the current branch.
- `write-a-prd`: when adding a new product surface (new wire adapter, new
  capability namespace, new upstream binding).
- `prd-to-plan`: when an accepted PRD needs phasing.
- `triage-issue`: when a user reports a facilitator bug.
- `tdd`: for `zpay-core` work where the test fixtures are small and the
  logic branches matter (prepare, oracle, compliance).

Do not invoke `improve-codebase-architecture` or `request-refactor-plan`
without an open issue from a maintainer. The architecture is freshly
recorded in the ADRs; uninvited refactoring is closed on sight.

## Tools

Prefer `Edit` for surgical changes; reserve `Write` for new files or full
rewrites. Never run `git push`, `gh pr create`, `gh pr review --approve`,
`gh pr review --request-changes`, or any destructive git command
(`reset --hard`, `clean -f`, `branch -D`, force-push) without explicit
confirmation in the same turn.

When investigating across the sibling repos, dispatch the `Explore`
subagent rather than reading every file inline. Cite file paths and line
numbers in your findings.

## Style reminders specific to zpay

- Never use em dashes in code, docs, comments, commits, or PR descriptions.
  Use colons, semicolons, parentheses, or restructure the sentence.
- Default to no code comments unless the why is non-obvious. Do not write
  comments that describe what the next few lines do; the code already does
  that.
- Do not add `Co-Authored-By: Claude` trailers on commits. The git history
  records the human owner; AI assistance is disclosed in the PR body.
- Do not add "Generated with Claude Code" footers.
- When you produce text outside of tool calls, narrate at the level of
  "what I'm about to do" and "what I found", not "what I'm thinking".

## When you are done

Mark all your tasks completed before ending the session. If you discovered
new follow-up work, file it as a GitHub issue rather than leaving it as a
stale task.
