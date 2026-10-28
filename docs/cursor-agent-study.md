# Cursor CLI agent study

This note studies the Cursor CLI agent (`cursor-agent`, also invoked as `agent`) and compares it with Grok Build.
Its goal is to pick the Cursor patterns that would most improve Grok's agent loop, tools, context handling, and terminal UX.

## Method and limits

The study uses three sources only:

- The installed CLI's own output. That covers `--help` for the root command and each subcommand, `about --format json`, and `status`, from version `2026.09.08-19d4164` at `~/.local/bin/cursor-agent`.
- Cursor's public CLI documentation at `cursor.com/docs/cli`. The pages read were overview, using, shell-mode, headless, reference/output-format, reference/slash-commands, reference/permissions, and reference/configuration.
- The shape of the CLI's on-disk state: the files under `~/.cursor/` and the top-level keys of `~/.cursor/cli-config.json`. No values beyond permissions and display flags were read.

The bundled JavaScript was not opened, decompiled, or copied.

Two observation attempts failed, and the study does not rely on them.
A headless `--print --output-format stream-json` run in a scratch repository failed with `Error: Authentication required. Please run 'agent login' first, or set CURSOR_API_KEY environment variable.`, even though `agent status` printed `Logged in (unable to fetch user details)`.
A `--force` run was refused by the local tool sandbox before it started.
Event shapes below therefore come from the published output-format reference, not from a captured stream.

## The agent loop

Cursor runs one loop in three entry points. These are an interactive TUI, `--print` for scripts and CI, and `agent acp`, which speaks the Agent Client Protocol over stdio.
The model call happens server side at `api2.cursor.sh`, and tools execute locally. `worker` mode inverts this. A cloud agent drives the loop and a local worker only executes tool calls.

Modes change what the loop may do, not how it runs.
Agent has every tool. Plan analyzes and proposes, and may ask clarifying questions. Ask is read-only Q&A.
Shift+Tab rotates the three, and `--mode plan|ask` or `/plan` and `/ask` select one directly.
Debug mode (`/debug`) and a long-lived objective (`/goal`, rolling out) are layered on top.

Approval is a separate axis from mode. `approvalMode` is `allowlist`, `auto-review`, or `unrestricted`.
`--force` (alias `--yolo`) runs every command unless a rule denies it.
`--auto-review` hands each tool call to a server-side classifier, which auto-runs safe calls and prompts for the rest.
In `--print` mode without `--force`, the docs say the agent only proposes file changes.

Long-running work leaves the terminal in three ways.
A message prefixed with `&` hands the conversation to a Cloud Agent.
`persist` keeps a session alive across terminal or SSH disconnects (`persist list|attach|stop`).
`-w/--worktree [name]` starts in a git worktree under `~/.cursor/worktrees/<repo>/<name>`, and runs setup scripts from `.cursor/worktrees.json` unless `--skip-worktree-setup` is passed.

## Tools

The public tool surface is files (read, write), search, shell, web fetch, and MCP.
The stream-json reference shows two structured tool records.
`readToolCall` returns `content`, `isEmpty`, `exceededLimit`, `totalLines`, and `totalChars`, so the model knows whether it saw the whole file.
`writeToolCall` returns `path`, `linesCreated`, and `fileSize`.

Permissions are one flat rule list in `~/.cursor/cli-config.json` (global) and `<project>/.cursor/cli.json` (project, permissions only):

```json
{ "permissions": { "allow": ["Shell(git)", "Read(src/**/*.ts)"], "deny": ["Shell(rm)", "Read(.env*)"] } }
```

There are five kinds: `Shell(commandBase)` with an optional `cmd:args` glob form, `Read(glob)`, `Write(glob)`, `WebFetch(*|*.domain|domain)`, and `Mcp(server:tool)`. A deny always beats an allow.
At an approval prompt, Tab adds the command to the allowlist.

Shell commands needing `sudo` get a masked password prompt. The password goes to `sudo` over IPC and the model never sees it.

## Context handling

Rules in `.cursor/rules` load by their own configuration. `AGENTS.md` and `CLAUDE.md` at the project root load alongside them.
`@` in the prompt attaches files and folders.
`/summarize` (alias `/compress`) compacts the conversation on demand.
`/rewind`, `/fork`, `/resume`, and `--continue` move through and branch history.
`--add-dir` adds extra workspace roots.
A configurable explore subagent model (`exploreSubagentModel` in the config) suggests that read-heavy search is delegated to a cheaper model.

## Terminal UX

- Shell Mode runs a command without leaving the chat. Output lands in the conversation and Ctrl+O expands truncated output. Commands time out at 30 seconds, and `cd` does not persist.
- Ctrl+R opens a review of the agent's changes. Left and right switch files, up and down scroll, and `i` adds a follow-up instruction.
- Newline is Shift+Enter in terminals that support it and Ctrl+J everywhere. `/setup-terminal` configures the binding.
- Exit takes a double Ctrl+D.
- `display.showStatusIndicators` puts status in the terminal title. `notifications` raises a terminal notification when the agent finishes or needs input. `display.showStatusLineRunningTime` shows elapsed time.
- `suggestNextPrompt` proposes a follow-up after each turn.
- `/copy-request-id`, `/copy-conversation-id`, and `/about` copy debugging context to the clipboard.
- `--model` accepts bracket parameters, as in `'claude-opus-4-8[context=1m,effort=high,fast=false]'`.

Headless output has three formats.
`text` is the final answer only.
`json` is one `result` object with `duration_ms`, `session_id`, and `request_id`.
`stream-json` is NDJSON with `system/init`, `user`, `assistant`, and `tool_call` events (`started` and `completed`, correlated by `call_id`), and a final `result`.
With `--stream-partial-output`, assistant deltas carry `timestamp_ms`. The pre-tool-call flush carries `model_call_id`, and consumers skip that flush to avoid duplicate text.

## Grok today

Each row was checked in this tree. Paths are under `crates/codegen/`.

| Cursor pattern | Grok | Evidence |
|---|---|---|
| Headless `text`/`json`/`stream-json` with `tool_call` events | Partial. Grok has `plain`, `json`, `streaming-json`, and Claude-shaped `streaming-messages-json`. Nothing emits Cursor's `tool_call` started/completed events. | `xai-grok-pager/src/headless/cli.rs` |
| Shell Mode | Present as `!`. The timeout is one hour, not 30 seconds. | `xai-grok-shell/src/session/acp_session_impl/tool_dispatch.rs:8` |
| `&` cloud handoff | Partial. Only on the Cursor backend. | `xai-grok-pager/src/acp/cursor/cloud.rs` |
| Agent/Plan/Ask rotation | Partial. Shift+Tab cycles permission modes. There is no native read-only Ask mode. | `xai-grok-pager/src/app/actions.rs` |
| Ctrl+R change review | Missing. Ctrl+R opens the session picker. | `xai-grok-pager/docs/user-guide/03-keyboard-shortcuts.md:253` |
| `/summarize` compaction | Different. `/summarize` is an alias of `/recap`, which prints a summary and does not compact. Compaction is `/compact`. | `xai-grok-pager/src/slash/commands/recap.rs:15` |
| Cursor permission files | Missing before this branch. Added by the port below. | `xai-grok-permission-rules/src/cursor_cli.rs` |
| Title, notifications, running time, suggestions | Present. | `xai-grok-pager/src/notifications/`, `xai-grok-pager/src/views/prompt_suggestion.rs` |
| Worktree setup scripts | Missing. `-w` exists. A `WorktreeCreate` hook is parsed and then dropped, and `.cursor/worktrees.json` is not read. | `xai-grok-hooks/src/config.rs:1448` |
| Read tool metadata | Partial. Only the total line count is returned, and truncation is a text marker. | `xai-grok-tools/src/types/output.rs` |
| Rules, `AGENTS.md`, `CLAUDE.md` | Present, behind folder trust. | `xai-grok-agent/src/prompt/agents_md.rs` |
| Masked sudo prompt | Partial. Works only when the user sets `SUDO_ASKPASS`. | `xai-grok-tools/src/computer/local/shell_state.rs` |
| Bracket model parameters | Partial. Supported on the Cursor backend. Native Grok uses `--effort`, `--context-window`, and `--fast`. | `xai-grok-pager/src/acp/cursor/adapter.rs` |

## What to bring over, ranked

1. **Cursor permission files.** A Cursor user's allowlist is the most direct carry-over. Without it, every command they already approved in Cursor prompts again in Grok. This lives in `xai-grok-permission-rules`. It is done on this branch.
2. **Worktree setup.** Run `.cursor/worktrees.json` setup commands, and make the parsed `WorktreeCreate` hook fire, when Grok creates a worktree. A fresh worktree without dependencies installed is the first thing the agent trips on.
3. **Read tool metadata.** Return `isEmpty`, `exceededLimit`, and `totalChars` as fields, not a text marker. The model then knows whether to page the rest of the file.
4. **`/summarize` and `/compress` as compaction.** A Cursor user typing `/summarize` expects the context to shrink. Today they get a recap and a full context window.
5. **Cursor-shaped `stream-json`.** Add `tool_call` started/completed events so scripts written against Cursor's headless output keep working.
6. **A change-review view** for the files a turn changed. This is the largest UX item. Ctrl+R is already taken, so it needs a new key or slash command.

## What was ported

Item 1 landed. Grok now reads `permissions.allow` and `permissions.deny` from `~/.cursor/cli-config.json`.
From a project's `.cursor/cli.json`, Grok takes only the deny rules, and only in a trusted folder.
Project allow rules are skipped, because the folder-trust gate in `xai-grok-workspace` does not yet count `.cursor/cli.json` as a trust-sensitive file. A cloned repository could otherwise approve its own commands.
An exact-host `WebFetch(example.com)` allow becomes two scheme-anchored URL globs. Grok's domain mode would also have allowed `evil.example.com`.
Grok matches `Bash` rules against the whole command line, while Cursor matches the first word. So an allow whose Grok form would be wider is skipped, as with a glob in the command name or exact `cmd:args` arguments. A deny keeps the wider form.
Relative `Read` and `Write` allows are rooted at the working directory, matching Cursor's workspace scoping.
The user guide documents the mapping in `22-permissions-and-safety.md`.

Items 2 through 6 live in `xai-grok-pager`, `xai-grok-tools`, `xai-grok-hooks`, or `xai-grok-shell`.
Each of those crates depends on `xai-grok-tools-api`, whose build script needs `protoc`. The repository resolves `protoc` through DotSlash (`bin/protoc`).
Neither DotSlash nor `protoc` was installed on the study machine, and fetching the pinned release was not permitted. Those crates could not be compiled, so changes to them were not attempted.
