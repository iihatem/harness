# AGENTS.md

Instructions for any AI coding agent working in this repo (Claude Code, Codex, Cursor, Gemini, etc.).
This file is the single source of truth; agent-specific files (e.g. `CLAUDE.md`) should only point here.

## Development workflow

Specs and change management use [OpenSpec](https://github.com/Fission-AI/OpenSpec) (`openspec/`).
Execution follows the Superpowers discipline: brainstorm → plan → TDD → review.

1. **Propose first.** Every non-trivial change starts as an OpenSpec change proposal
   (`openspec/changes/<name>/`: proposal, spec deltas, design, tasks). Do not implement
   until the human has approved the proposal.
2. **Implement with discipline.** Write a plan before touching code, write failing tests
   before the code that makes them pass, and get a review before calling anything done.
   If your agent has the Superpowers skills (writing-plans, test-driven-development,
   requesting-code-review, verification-before-completion), use them; otherwise follow the same steps by hand.
3. **Archive when done.** Once the tasks are complete and verified, archive the change
   (`openspec archive <name>`) so the main specs in `openspec/specs/` stay current.
4. **Trivial changes skip the proposal.** Typos, copy tweaks, and dependency bumps can go
   straight to implementation (still verify before finishing).
5. **No Spec Kit.** Do not create GitHub Spec Kit artifacts (`.specify/`, `/speckit.*` commands, constitution files).

## OpenSpec entry points

| Agent | Propose | Implement | Archive |
|---|---|---|---|
| Claude Code | `/opsx:propose "<idea>"` | `/opsx:apply` | `/opsx:archive` |
| Others (skills in `.agents/skills/`) | `openspec-propose` skill | `openspec-apply-change` skill | `openspec-archive-change` skill |
| Any (CLI) | `openspec new change <name>` | `openspec instructions apply --change <name>` | `openspec archive <name>` |

Useful CLI: `openspec list`, `openspec show <name>`, `openspec validate <name>`, `openspec status --change <name>`, `openspec view`.

To add another agent's native integration later: `openspec update` / `openspec init --tools <tool>`.
