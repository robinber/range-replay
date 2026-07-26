# kira-mux command reference (range-replay)

Canonical binary: `kira-mux`. Prefer live help when flags differ by install:

```bash
kira-mux --help
kira-mux <command> --help
```

## Project selection

| Target | Meaning |
|---|---|
| `range-replay` | Explicit project id from `~/.config/kira-mux/projects/range-replay.toml` |
| `.` | Deepest configured root containing the current working directory |

Example from a subdirectory:

```bash
cd ~/Desktop/Projects/rust/range-replay/src
kira-mux status .
kira-mux send . codex "…"
```

## Lifecycle

| Command | Purpose |
|---|---|
| `list` | Configured projects and live state |
| `open <project>` | Create/repair workspace and attach |
| `start <project>` | Create/repair without attach |
| `attach <project>` | Attach to existing session |
| `status <project>` | Workspace + agent pane state |
| `agents list <project>` | Agent table (command, state, capabilities) |
| `restart <project> [agent]` | Restart all agents or one id |
| `kill <project> --yes` | Tear down managed session |

range-replay currently has **no profiles**. If a future config adds
`[profiles.<name>]`, pass `--profile <name>` on commands that accept it.

## Send

```bash
kira-mux send <project> <agent> <prompt>
  [--profile <profile>]
  [--no-template]
  [--from <from>]          # default: user
  [--trace-id <id>]
  [--thread <thread>]
```

- `<agent>` for range-replay: `claude` | `codex` | `grok`
- Prompt is delivered to a **live** pane; readiness is not checked
- Use heredocs for multi-line tasks
- `--from`, `--thread`, and `--trace-id` support orchestration bookkeeping when
  the install has thread/msgbus features enabled

## Capture

```bash
kira-mux capture <project> <agent>
  [--lines <n>]            # default: 30
  [--json]
  [--profile <profile>]
  [--save-thread <thread>]
  [--save-profile <save_profile>]
  [--trace-id <id>]
```

Use enough `--lines` to cover the reply. Prefer saving thread evidence when
coordination requires an audit trail.

## Config locations

| Path | Role |
|---|---|
| `~/.config/kira-mux/config.toml` | Global defaults and agent templates |
| `~/.config/kira-mux/projects/range-replay.toml` | range-replay agents and root |
| `.kira/range-replay.toml` | optional in-repo draft reference |

## Drift

Fingerprint includes project id, profile id, root, layout, main pane ratio,
window name, shell/remain-on-exit defaults, and per-agent mode, command,
shell_command, args, cwd, and env (literal values hashed). Mismatch → drifted
session; fix with `kill` then `open`/`start`.

Excluded from fingerprint: display `name`/`label`, `capabilities`, `groups`,
`prompt_template`.

## range-replay agent launch args

From the active project file (verify before relying on memory):

```toml
# claude
args = ["--dangerously-skip-permissions"]

# codex
args = ["-a", "never", "-s", "danger-full-access"]

# grok
args = ["--always-approve", "--permission-mode", "bypassPermissions"]
```

## Related repo docs

- `AGENTS.md` — Kira orchestration rules and completion checklist
- `README.md` — product scope and learning workflow
- `.agents/skills/rust-strict/SKILL.md` — required for Rust work on any pane
