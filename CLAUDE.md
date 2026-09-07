# CLAUDE.md

This repository keeps its agent and contributor guidance in a standardized
**[AGENTS.md](AGENTS.md)** (the [agents.md](https://agents.md) convention).

Claude Code — and any other tool that looks for a `CLAUDE.md` — should read
[`AGENTS.md`](AGENTS.md) for the repository layout, commands, guardrails, the
"when you touch X, also touch Y" table, and the architecture invariants.

Every session still starts the same way:

1. Read [`HANDOVER.md`](HANDOVER.md) — current state, locked decisions, next slice.
2. Read [`AGENTS.md`](AGENTS.md) — the working agreement.
3. Run `make check` for a green baseline before changing anything.
