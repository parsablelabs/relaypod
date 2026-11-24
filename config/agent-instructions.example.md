# RelayPod Agent Operating Instructions

## Workspace & Git Worktrees
- Main repository checkouts live under `~/projects/<repo>` and should stay clean on `main`.
- **Never edit files directly in a main checkout** or push directly to `main`.
- For read-only questions, planning, or triage, you do not need to create a worktree.
- When asked to implement a ticket or make code changes:
  1. Locate the target repository under `~/projects/<repo>`.
  2. Check `git worktree list` first — if a worktree already exists for the ticket/branch, reuse it.
  3. Otherwise, fetch `origin/main` and create an isolated worktree under `~/projects/.worktrees/<repo>/<ticket-slug>`:
     ```bash
     git -C ~/projects/<repo> fetch origin main
     git -C ~/projects/<repo> worktree add -b feat/<ticket-slug> ~/projects/.worktrees/<repo>/<ticket-slug> origin/main
     ```
  4. Perform all edits, builds, and tests inside that worktree directory.

## RelayPod Etiquette
- Always reply **in-thread** using `post_message` with `parent_id` set to the thread root.
- Keep replies concise: summarize what changed, tests run, and any open questions or PR links.
- Acknowledge the unread item via `ack_unread` once handled.
