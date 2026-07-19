# Script tasks with AI remediation

## Context

Add a new task mode to amake: tasks with a `script` field that runs a bash command directly (bypassing AI). When the script fails, the task can optionally dispatch an AI remediation prompt — either as a one-shot fix (`--fix`) or as a fix-and-reverify loop (`--redo`). This lets developers keep the fast deterministic path for normal cases and only pay the AI cost when something breaks.

## Design overview

### Task definition

```toml
[tasks.build]
script = "cargo build"
prompt = "Fix the compilation error. Print a report of changes."
autofix = true
autoredo = true
```

- `script` (optional `String`) — bash command to run. Supports `{{...}}` template interpolation (vars, env, task outputs), just like `prompt`.
- `prompt` becomes **optional** when `script` is present. If `script` is absent, `prompt` is required (current behavior).
- `autofix` / `autoredo` (optional `bool`) — default fix/redo mode, overridable by CLI flags.

### Execution modes

| `amake run build` | No AI. Pure bash alias. Script failure = task failure. |
|---|---|
| `amake run build --fix` | Run script. On failure, build context (`command`, `exit code`, `stdout`, `stderr`) is appended to `prompt` and dispatched to the AI tool. AI runs once, task is done. |
| `amake run build --redo` | Same as `--fix`, but after AI finishes, re-run the script. Loop until success or max 3 attempts. |



### Auto-injected context

When the script fails and the AI is about to be called, the prompt is automatically augmented with:

```
Command: cargo build
Exit code: 101
stdout:
[stdout content if any]

stderr:
[stderr content]
```

This is appended to the user's `prompt` text so the AI sees exactly what went wrong.

### Redo loop

- Hardcoded max 3 attempts (including the initial run).
- The loop: run script → on success ✅ done → on failure → build context → dispatch AI → re-run script → ...
- The final successful run's **stdout** is what gets captured for `{{tasks.name.stdout}}` / `capture = true`.
- If all 3 attempts fail, the task fails with a `RedoMaxAttempts` error. The error **includes the last script run's exit code and stderr tail** (not just a generic "max attempts" message).

## Files to modify

- **`src/config.rs`** — schema changes: make `prompt` optional, add `script`, `autofix`, `autoredo`
- **`src/error.rs`** — new error variants for validation and redo exhaustion
- **`src/runner.rs`** — new execution path for script tasks, fix/redo logic, context building
- **`src/main.rs`** — add `--fix` and `--redo` CLI flags
- **`README.md`** — document the new feature

## Reuse

- **Adapter system** (`src/adapter/`) — the remediation dispatch reuses `Adapter::build_command()` exactly as today, with the augmented prompt as the task's `prompt`.
- **Template rendering** (`src/template.rs`) — the remediation prompt is rendered (so `{{vars.*}}`, `{{env.*}}`, `{{tasks.*}}` work inside it). The auto-injected context is added *after* rendering.
- **Child process management** (`src/runner.rs`) — script execution reuses the same timeout/idle-monitor/signal handling as regular tasks.
- **Sandbox** (`src/adapter/sandbox.rs`) — if sandbox is configured, the script runs inside clampdown too (wrapping `sh -c <script>`).

## Implementation steps

### Step 1: Schema updates (`src/config.rs`)

- Make `prompt` in `RawTask` optional: `prompt: Option<String>` (it was `String`).
- Add `script: Option<String>`, `autofix: bool`, `autoredo: bool` to `RawTask` (with `#[serde(default)]`).
- Propagate to the public `Task` struct.
- Add validation in `Config::from_str`:
  - If `script` is `None` and `prompt` is `None` → error (task must have at least one).
  - If `script` is `Some` and (`autofix` or `autoredo`) and `prompt` is `None` → error.
  - If `script` is `None` and (`autofix` or `autoredo`) → error.

### Step 2: CLI flags (`src/main.rs`)

- Add `--fix` (long flag) and `--redo` (long flag) to the `Run` subcommand.
- Pass them through to `RunOptions` in the runner.

### Step 3: Error variants (`src/error.rs`)

- `ScriptTaskNoPrompt { task }` — autofix/autoredo but no prompt.
- `NotAScriptTask { task }` — autofix/autoredo on a non-script task.
- `TaskNoPromptOrScript { task }` — task has neither.
- `FixNotAScriptTask { task }` — `--fix`/`--redo` on a non-script task.
- `FixNoPrompt { task }` — `--fix`/`--redo` on a script task without prompt.
- `RedoMaxAttempts { task, attempts }` — redo loop exhausted.

### Step 4: Runner logic (`src/runner.rs`)

The core change is in the main task execution loop. For each task:

1. **Determine if script task**: `task.script.is_some()`.
2. **Determine fix/redo mode**: CLI flag wins over `autofix`/`autoredo` in config. Use an enum `FixMode { Off, Fix, Redo }`.
3. **Validate**: if fix/redo mode is active but task has no script → `FixNotAScriptTask`. If task has script but no prompt → `FixNoPrompt`.
4. **Execute script** (if script task):
   - Render `script` through the template engine (same `render()` call used for `prompt`), so `{{vars.*}}`, `{{env.*}}`, `{{tasks.*}}` work inside the script.
   - Build `Command::new("sh").arg("-c").arg(&rendered_script)`.
   - Apply sandbox if configured (wrap in `clampdown`).
   - Apply workdir if configured.
   - Apply timeout, idle monitoring.
   - Run with capture of stdout + stderr.
5. **On success**: capture stdout, continue to next task.
6. **On failure in fix/redo mode**:
   - Build context string: `Command: {script}\nExit code: {code}\nstdout:\n{stdout}\nstderr:\n{stderr}`
   - Augment prompt: `{rendered_prompt}\n\n---\n{context}`
   - Dispatch to AI adapter with augmented prompt.
   - If `Redo` mode: loop back to step 4 (max 3 total attempts).
   - If `Fix` mode: task is done (success).
7. **On failure with fix/redo disabled**: return error as today.

### Step 5: Tests

- Unit tests in `config.rs` for parsing/validation.
- Unit tests in `runner.rs` for fix/redo execution paths.
- Update existing tests that assume `prompt` is always present.

### Step 6: Documentation (`README.md`)

- Document `script`, `autofix`, `autoredo`.
- Document `--fix` and `--redo` CLI flags.
- Include the mode selection precedence table (the one removed from this plan) in the README so users understand the behavior matrix.
- Show examples.

## Verification

1. `cargo build` — compiles cleanly.
2. `cargo test` — all existing tests pass, plus new ones.
3. Manual smoke test with a real Amakefile:

```toml
[tasks.greet]
script = "echo hello && exit 1"
prompt = "fix this"
autofix = true
tool = "pi"
```

```bash
amake run greet          # should fail (script fails, no fix because autofix needs CLI... wait)
```

Actually — need to clarify the default behavior. Let me ask.
