# Coming from Claude, Cursor, or Codex?

Fear not — your settings, rules, and skills come with you. Grok Build
reads the same project conventions other agents use, and imports the rest.

## Picked up automatically

- **Rules & instructions** — `AGENTS.md` (the Codex/OpenCode convention),
  `CLAUDE.md` (including nested ones), and `*.md` rules under
  `.claude/rules/` and `.cursor/rules/`.
- **Skills & custom commands** — `~/.claude/skills/`, `~/.claude/commands/`,
  `~/.cursor/skills/`, and their project-level twins. Flat command `.md`
  files become slash commands here too.
- **MCP servers** — from `~/.claude.json`, `.cursor/mcp.json`, and project
  `.mcp.json`.
- **Hooks** — from `.claude/settings.json`, including matcher aliases like
  `Bash`, so most hooks run unchanged.

## One-step import

**`/import-claude`** scans your `~/.claude` settings — permissions, env
vars, MCP servers, hooks — and shows a checkbox preview; confirming
writes the items you selected into your `.grok` config. Re-run it anytime.

## Pick up where you left off

Ctrl-R lists recent Cursor, Claude, and Codex sessions for this workspace.
Cursor CLI chats load directly. Cursor IDE sidepane chats import when the
grok backend can read the transcript (`cursor-agent` cannot resume those).
**`/resume-claude`** and **`/resume-codex`** are the fallback when a
transcript is not on disk.

## Check what was discovered

Run **`grok inspect`** in a repo to see every rules file, skill, and MCP
server Grok picked up, tagged with where it came from. Each compat source
can be toggled in `[compat.claude]` / `[compat.cursor]` config sections.

And a few things you might have missed elsewhere: `/btw` asks a side
question without interrupting the current task, and `/rewind` rewinds the
conversation to an earlier turn (file changes stay as they are).

*Go deeper: `/docs Project Rules (AGENTS.md)`, `/docs Skills`, or `/docs MCP Servers`*
