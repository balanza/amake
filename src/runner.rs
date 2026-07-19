use crate::adapter::AdapterRegistry;
use crate::config::{BackoffStrategy, Config, RetryConfig};
use crate::error::Error;
use crate::render::{self, Assets, StreamingRenderer};
use crate::report::{self, Activity};
use crate::sandbox::SandboxConfig;
use crate::template;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{BufRead, BufReader, Write as _};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use wait_timeout::ChildExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FixMode {
    Off,
    Fix,
    Redo,
}

#[derive(Clone)]
enum RenderMode {
    Off,
    On(Arc<Assets>),
}

pub fn resolve_execution_order(config: &Config, targets: &[String]) -> Result<Vec<String>, Error> {
    for target in targets {
        if !config.tasks.contains_key(target) {
            return Err(Error::UnknownTask(target.clone()));
        }
    }

    let mut needed: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<String> = targets.iter().cloned().collect();

    while let Some(name) = queue.pop_front() {
        if needed.contains(&name) {
            continue;
        }
        let task = config
            .tasks
            .get(&name)
            .ok_or_else(|| Error::UnknownTask(name.clone()))?;
        needed.insert(name);
        for dep in &task.depends {
            queue.push_back(dep.clone());
        }
    }

    let mut in_degree: BTreeMap<&str, usize> = needed
        .iter()
        .map(|name| {
            let deps_count = config.tasks[name]
                .depends
                .iter()
                .filter(|d| needed.contains(*d))
                .count();
            (name.as_str(), deps_count)
        })
        .collect();

    let mut queue: VecDeque<&str> = in_degree
        .iter()
        .filter(|(_, deg)| **deg == 0)
        .map(|(&name, _)| name)
        .collect();

    let mut order = Vec::with_capacity(needed.len());

    while let Some(name) = queue.pop_front() {
        order.push(name.to_string());

        let dependents: Vec<&str> = needed
            .iter()
            .filter(|n| config.tasks[*n].depends.iter().any(|d| d == name))
            .map(|n| n.as_str())
            .collect();

        for dep in dependents {
            if let Some(deg) = in_degree.get_mut(dep) {
                *deg -= 1;
                if *deg == 0 {
                    queue.push_back(dep);
                }
            }
        }
    }

    if order.len() != needed.len() {
        let remaining: Vec<&str> = needed
            .iter()
            .filter(|n| !order.contains(n))
            .map(|n| n.as_str())
            .collect();
        return Err(Error::DependencyCycle(remaining.join(" -> ")));
    }

    Ok(order)
}

fn check_clampdown() -> Result<(), Error> {
    which("clampdown").ok_or(Error::ClampdownNotFound)
}

fn which(binary: &str) -> Option<()> {
    std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(binary))
                .find(|path| path.is_file())
        })
        .map(|_| ())
}

pub struct RunOptions {
    pub dry_run: bool,
    pub keep_going: bool,
    pub force_sandbox: bool,
    pub no_sandbox: bool,
    pub no_format: bool,
    pub fix: bool,
    pub redo: bool,
    pub vars: BTreeMap<String, String>,
}

pub fn run(config: &Config, targets: &[String], opts: &RunOptions) -> Result<(), Error> {
    let order = resolve_execution_order(config, targets)?;

    let render_mode = if render::should_render(opts.no_format) {
        RenderMode::On(Arc::new(Assets::load()))
    } else {
        RenderMode::Off
    };

    let capture_flags: BTreeMap<String, bool> = config
        .tasks
        .iter()
        .map(|(name, task)| (name.clone(), task.capture))
        .collect();

    let registry = AdapterRegistry::new();
    let mut task_outputs: BTreeMap<String, String> = BTreeMap::new();
    let mut sandbox_checked = false;
    let mut failures: Vec<String> = Vec::new();

    for task_name in &order {
        let task = &config.tasks[task_name];
        let workdir = config.effective_workdir(task);
        let sandbox = config.effective_sandbox(task, opts.force_sandbox, opts.no_sandbox);
        let timeout = config.effective_timeout(task);
        let retry = config.effective_retry(task);
        let idle_warn = config.effective_idle_warn(task);
        let idle_kill = config.effective_idle_kill(task);

        if sandbox.is_some() && !sandbox_checked {
            check_clampdown()?;
            sandbox_checked = true;
        }

        let fix_mode = resolve_fix_mode(task, opts);

        if task.script.is_some() {
            // --- Script task path ---
            run_script_task(
                config,
                task,
                task_name,
                opts,
                &render_mode,
                &registry,
                &mut task_outputs,
                &capture_flags,
                workdir.as_deref(),
                sandbox.as_ref(),
                timeout,
                retry.as_ref(),
                idle_warn,
                idle_kill,
                fix_mode,
                &mut failures,
            )?;
        } else {
            // --- Normal AI task path ---
            run_ai_task(
                config,
                task,
                task_name,
                opts,
                &render_mode,
                &registry,
                &mut task_outputs,
                &capture_flags,
                workdir.as_deref(),
                sandbox.as_ref(),
                timeout,
                retry.as_ref(),
                idle_warn,
                idle_kill,
                &mut failures,
            )?;
        }
    }

    if !failures.is_empty() {
        report::status_line(&format!(
            "\n✗ {} task(s) failed: {}",
            failures.len(),
            failures.join(", ")
        ));
        return Err(Error::TaskFailed {
            task: failures.join(", "),
            code: 1,
            attempts: 1,
            command: None,
            stderr_tail: None,
        });
    }

    Ok(())
}

fn resolve_fix_mode(task: &crate::config::Task, opts: &RunOptions) -> FixMode {
    if opts.redo {
        FixMode::Redo
    } else if opts.fix {
        FixMode::Fix
    } else if task.autoredo {
        FixMode::Redo
    } else if task.autofix {
        FixMode::Fix
    } else {
        FixMode::Off
    }
}

/// Run a normal AI task (existing behavior).
fn run_ai_task(
    config: &Config,
    task: &crate::config::Task,
    task_name: &str,
    opts: &RunOptions,
    render_mode: &RenderMode,
    registry: &AdapterRegistry,
    task_outputs: &mut BTreeMap<String, String>,
    capture_flags: &BTreeMap<String, bool>,
    workdir: Option<&Path>,
    sandbox: Option<&SandboxConfig>,
    timeout: Option<Duration>,
    retry: Option<&RetryConfig>,
    idle_warn: Option<Duration>,
    idle_kill: Option<Duration>,
    failures: &mut Vec<String>,
) -> Result<(), Error> {
    let tool = config.effective_tool(task_name)?;

    let rendered_prompt = template::render(
        task.prompt.as_deref().unwrap_or(""),
        task_name,
        task_outputs,
        &opts.vars,
        &task.depends,
        capture_flags,
    )?;

    let mut rendered_task = task.clone();
    rendered_task.prompt = Some(rendered_prompt);
    rendered_task.model = config.effective_model(task);

    let resolved = registry.resolve_or_generic(&tool);
    let adapter = resolved.adapter();

    let auto_approve = task.auto_approve;
    let cmd_builder = || adapter.build_command(&rendered_task, workdir, auto_approve, sandbox);

    if opts.dry_run {
        let cmd = cmd_builder();
        print_command(task_name, &cmd, timeout, retry);
        return Ok(());
    }

    report::status_line(&format!("▶ running task: {task_name} (tool: {tool})"));

    let cmd_string = format_command(&cmd_builder());

    let attempt_opts = AttemptOpts {
        capture: task.capture,
        timeout,
        render_mode,
        idle_warn,
        idle_kill,
    };
    let (result, attempts) =
        execute_with_retry(task_name, cmd_builder, &attempt_opts, retry)?;

    handle_task_result(
        result,
        attempts,
        task_name,
        opts.keep_going,
        Some(cmd_string),
        timeout,
        task_outputs,
        failures,
    )
}

/// Run a script task. The script runs as `sh -c <script>`. On failure,
/// if fix/redo mode is active, the AI is dispatched with the failure context.
fn run_script_task(
    config: &Config,
    task: &crate::config::Task,
    task_name: &str,
    opts: &RunOptions,
    render_mode: &RenderMode,
    registry: &AdapterRegistry,
    task_outputs: &mut BTreeMap<String, String>,
    capture_flags: &BTreeMap<String, bool>,
    workdir: Option<&Path>,
    sandbox: Option<&SandboxConfig>,
    timeout: Option<Duration>,
    retry: Option<&RetryConfig>,
    idle_warn: Option<Duration>,
    idle_kill: Option<Duration>,
    fix_mode: FixMode,
    failures: &mut Vec<String>,
) -> Result<(), Error> {
    let has_prompt = task.prompt.is_some();

    // If no explicit prompt, use a default one for remediation.
    let default_prompt = "The script failed. Fix the issue.";
    let prompt_source = task.prompt.as_deref().unwrap_or(default_prompt);

    // Render the script through the template engine.
    let rendered_script = template::render(
        task.script.as_deref().unwrap_or(""),
        task_name,
        task_outputs,
        &opts.vars,
        &task.depends,
        capture_flags,
    )?;

    // Resolve tool only if we might dispatch to AI (fix/redo mode with a prompt).
    let tool = if fix_mode != FixMode::Off && has_prompt {
        Some(config.effective_tool(task_name)?)
    } else {
        None
    };

    let max_redo_attempts: u32 = 3;
    let script_builder = || build_script_command(&rendered_script, workdir, sandbox);

    for redo_attempt in 1..=max_redo_attempts {
        // --- Run the script ---
        if opts.dry_run {
            let cmd = script_builder();
            print_command(task_name, &cmd, timeout, retry);
            return Ok(());
        }

        let phase = if redo_attempt == 1 {
            String::new()
        } else {
            format!(" (redo attempt {redo_attempt}/{max_redo_attempts})")
        };
        report::status_line(&format!("▶ running script task: {task_name}{phase}"));

        let attempt_opts = AttemptOpts {
            capture: task.capture,
            timeout,
            render_mode,
            idle_warn,
            idle_kill,
        };
        let (result, _script_attempts) =
            execute_with_retry(task_name, script_builder, &attempt_opts, retry)?;

        // Determine exit code and stderr tail from the result.
        let (exit_code, stderr_tail): (i32, String) = match &result {
            TaskResult::Success(_) => {
                // Script succeeded — we're done.
                if let Some(stdout) = result.into_stdout() {
                    task_outputs.insert(task_name.to_string(), stdout);
                }
                return Ok(());
            }
            TaskResult::Failed(code, stderr) => (*code, stderr.clone()),
            TaskResult::Signaled(stderr) => (-1, stderr.clone()),
            TaskResult::TimedOut(stderr) => (-1, stderr.clone()),
            TaskResult::IdleKilled {
                stderr_tail,
                ..
            } => (-1, stderr_tail.clone()),
        };

        if fix_mode == FixMode::Off {
            // No fix mode — fail immediately.
            return handle_failure_outright(
                task_name, &result, opts.keep_going, timeout, failures,
            );
        }

        // --- Fix/redo mode: dispatch remediation to AI ---
        let context = format!(
            "Command: {}\nExit code: {}\nstdout:\n{}\n\nstderr:\n{}",
            task.script.as_deref().unwrap_or(""),
            exit_code,
            task_outputs.get(task_name).map(|s| s.as_str()).unwrap_or(""),
            stderr_tail,
        );

        let rendered_prompt = template::render(
            prompt_source,
            task_name,
            task_outputs,
            &opts.vars,
            &task.depends,
            capture_flags,
        )?;
        let augmented_prompt = format!("{rendered_prompt}\n\n---\n{context}");

        let tool = tool.as_ref().expect("tool must be resolved in fix/redo mode");

        let mut remediate_task = task.clone();
        remediate_task.prompt = Some(augmented_prompt);
        remediate_task.model = config.effective_model(task);

        let resolved = registry.resolve_or_generic(tool);
        let adapter = resolved.adapter();
        let auto_approve = task.auto_approve;
        let remediate_cmd_builder =
            || adapter.build_command(&remediate_task, workdir, auto_approve, sandbox);

        report::status_line(&format!(
            "▶ dispatching remediation for task: {task_name} (tool: {tool})"
        ));

        let remediate_opts = AttemptOpts {
            capture: false,
            timeout,
            render_mode,
            idle_warn,
            idle_kill,
        };
        let (remediate_result, _remediate_attempts) = execute_with_retry(
            task_name,
            remediate_cmd_builder,
            &remediate_opts,
            retry,
        )?;

        match remediate_result {
            TaskResult::Success(_) => {
                if fix_mode == FixMode::Fix {
                    // Fix mode: AI fixed it, we're done.
                    return Ok(());
                }
                // Redo mode: loop back to re-run the script.
                continue;
            }
            _ => {
                // Remediation itself failed.
                return handle_failure_outright(
                    &format!("{task_name} (remediation)"),
                    &remediate_result,
                    opts.keep_going,
                    timeout,
                    failures,
                );
            }
        }
    }

    // All redo attempts exhausted.
    Err(Error::RedoMaxAttempts {
        task: task_name.to_string(),
        attempts: max_redo_attempts,
        code: -1,
        stderr_tail: "all attempts failed".into(),
    })
}

/// Build a `Command` to run a script via `sh -c`.
fn build_script_command(
    script: &str,
    workdir: Option<&Path>,
    sandbox: Option<&SandboxConfig>,
) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(script);

    // Apply sandbox if configured (wrap in clampdown).
    if let Some(sb) = sandbox {
        // Wrap the command in clampdown: clampdown sh -c <script>
        let mut clampdown_cmd = Command::new("clampdown");
        clampdown_cmd.arg("sh");
        for arg in sb.to_args() {
            clampdown_cmd.arg(arg);
        }
        if let Some(dir) = workdir {
            clampdown_cmd.arg("--workdir").arg(dir);
        }
        clampdown_cmd.arg("--");
        clampdown_cmd.arg("-c").arg(script);
        return clampdown_cmd;
    }

    if let Some(dir) = workdir {
        cmd.current_dir(dir);
    }

    cmd
}

/// Handle a failure when there's no fix/redo mode (or remediation failed).
/// Returns `Err` unless `keep_going` is set, in which case it logs and returns `Ok`.
fn handle_failure_outright(
    task_name: &str,
    result: &TaskResult,
    keep_going: bool,
    timeout: Option<Duration>,
    failures: &mut Vec<String>,
) -> Result<(), Error> {
    match result {
        TaskResult::Success(_) => {
            // Shouldn't happen, but treat as success.
            Ok(())
        }
        TaskResult::Failed(code, stderr_tail) => {
            if keep_going {
                report::status_line(&format!(
                    "✗ task {task_name:?} failed (exit code {code}), continuing..."
                ));
                failures.push(task_name.to_string());
                Ok(())
            } else {
                Err(Error::TaskFailed {
                    task: task_name.to_string(),
                    code: *code,
                    attempts: 1,
                    command: None,
                    stderr_tail: Some(stderr_tail.clone()),
                })
            }
        }
        TaskResult::Signaled(stderr_tail) => {
            if keep_going {
                report::status_line(&format!(
                    "✗ task {task_name:?} was killed by a signal, continuing..."
                ));
                failures.push(task_name.to_string());
                Ok(())
            } else {
                Err(Error::TaskSignaled {
                    task: task_name.to_string(),
                    attempts: 1,
                    command: None,
                    stderr_tail: Some(stderr_tail.clone()),
                })
            }
        }
        TaskResult::TimedOut(stderr_tail) => {
            let secs = timeout.map(|d| d.as_secs()).unwrap_or(0);
            if keep_going {
                report::status_line(&format!(
                    "✗ task {task_name:?} timed out after {secs}s, continuing..."
                ));
                failures.push(task_name.to_string());
                Ok(())
            } else {
                Err(Error::TaskTimeout {
                    task: task_name.to_string(),
                    timeout_secs: secs,
                    attempts: 1,
                    command: None,
                    stderr_tail: Some(stderr_tail.clone()),
                })
            }
        }
        TaskResult::IdleKilled {
            stderr_tail,
            idle_secs,
            idle_kill_secs,
        } => {
            if keep_going {
                report::status_line(&format!(
                    "✗ task {task_name:?} killed after {idle_secs}s of silence (limit {idle_kill_secs}s), continuing..."
                ));
                failures.push(task_name.to_string());
                Ok(())
            } else {
                Err(Error::TaskIdleKilled {
                    task: task_name.to_string(),
                    idle_secs: *idle_secs,
                    idle_kill_secs: *idle_kill_secs,
                    attempts: 1,
                    command: None,
                    stderr_tail: Some(stderr_tail.clone()),
                })
            }
        }
    }
}

/// Handle a task result (Success/Failed/Signaled/TimedOut/IdleKilled).
fn handle_task_result(
    result: TaskResult,
    attempts: u32,
    task_name: &str,
    keep_going: bool,
    cmd_string: Option<String>,
    timeout: Option<Duration>,
    task_outputs: &mut BTreeMap<String, String>,
    failures: &mut Vec<String>,
) -> Result<(), Error> {
    match result {
        TaskResult::Success(output) => {
            if let Some(stdout) = output {
                task_outputs.insert(task_name.to_string(), stdout);
            }
            Ok(())
        }
        TaskResult::Failed(code, stderr_tail) => {
            if keep_going {
                report::status_line(&format!(
                    "✗ task {task_name:?} failed (exit code {code}), continuing..."
                ));
                failures.push(task_name.to_string());
                Ok(())
            } else {
                Err(Error::TaskFailed {
                    task: task_name.to_string(),
                    code,
                    attempts,
                    command: cmd_string,
                    stderr_tail: Some(stderr_tail),
                })
            }
        }
        TaskResult::Signaled(stderr_tail) => {
            if keep_going {
                report::status_line(&format!(
                    "✗ task {task_name:?} was killed by a signal, continuing..."
                ));
                failures.push(task_name.to_string());
                Ok(())
            } else {
                Err(Error::TaskSignaled {
                    task: task_name.to_string(),
                    attempts,
                    command: cmd_string,
                    stderr_tail: Some(stderr_tail),
                })
            }
        }
        TaskResult::TimedOut(stderr_tail) => {
            let timeout_secs = timeout.map(|d| d.as_secs()).unwrap_or(0);
            if keep_going {
                report::status_line(&format!(
                    "✗ task {task_name:?} timed out after {timeout_secs}s, continuing..."
                ));
                failures.push(task_name.to_string());
                Ok(())
            } else {
                Err(Error::TaskTimeout {
                    task: task_name.to_string(),
                    timeout_secs,
                    attempts,
                    command: cmd_string,
                    stderr_tail: Some(stderr_tail),
                })
            }
        }
        TaskResult::IdleKilled {
            stderr_tail,
            idle_secs,
            idle_kill_secs,
        } => {
            if keep_going {
                report::status_line(&format!(
                    "✗ task {task_name:?} killed after {idle_secs}s of silence (idle limit {idle_kill_secs}s), continuing..."
                ));
                failures.push(task_name.to_string());
                Ok(())
            } else {
                Err(Error::TaskIdleKilled {
                    task: task_name.to_string(),
                    idle_secs,
                    idle_kill_secs,
                    attempts,
                    command: cmd_string,
                    stderr_tail: Some(stderr_tail),
                })
            }
        }
    }
}

enum TaskResult {
    Success(Option<String>),
    Failed(i32, String),
    Signaled(String),
    TimedOut(String),
    IdleKilled {
        stderr_tail: String,
        idle_secs: u64,
        idle_kill_secs: u64,
    },
}

impl TaskResult {
    fn into_stdout(self) -> Option<String> {
        match self {
            TaskResult::Success(stdout) => stdout,
            _ => None,
        }
    }
}

struct AttemptOpts<'a> {
    capture: bool,
    timeout: Option<Duration>,
    render_mode: &'a RenderMode,
    idle_warn: Option<Duration>,
    idle_kill: Option<Duration>,
}

fn execute_with_retry(
    task_name: &str,
    cmd_builder: impl Fn() -> std::process::Command,
    opts: &AttemptOpts,
    retry: Option<&RetryConfig>,
) -> Result<(TaskResult, u32), Error> {
    let max_attempts = retry.map(|r| r.attempts).unwrap_or(1).max(1);
    let on_timeout_retry = retry.map(|r| r.on_timeout).unwrap_or(true);

    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        let cmd = cmd_builder();
        let result = execute_attempt(task_name, cmd, opts)?;

        let should_retry = match &result {
            TaskResult::Success(_) => false,
            TaskResult::TimedOut(_) | TaskResult::IdleKilled { .. } => {
                on_timeout_retry && attempt < max_attempts
            }
            TaskResult::Failed(_, _) | TaskResult::Signaled(_) => attempt < max_attempts,
        };

        if !should_retry {
            return Ok((result, attempt));
        }

        let cfg = retry.expect("retry must be Some when attempt < max_attempts");
        let delay = compute_backoff(cfg, attempt);
        let kind = describe_failure(&result, opts.timeout);
        report::status_line(&format!(
            "⟲ task {task_name:?} {kind} (attempt {attempt}/{max_attempts}), retrying in {}s...",
            delay.as_secs()
        ));
        std::thread::sleep(delay);
    }
}

fn describe_failure(result: &TaskResult, timeout: Option<Duration>) -> String {
    match result {
        TaskResult::Failed(code, _) => format!("failed (exit {code})"),
        TaskResult::Signaled(_) => "was killed by a signal".to_string(),
        TaskResult::TimedOut(_) => {
            let secs = timeout.map(|d| d.as_secs()).unwrap_or(0);
            format!("timed out after {secs}s")
        }
        TaskResult::IdleKilled {
            idle_secs,
            idle_kill_secs,
            ..
        } => format!("went idle for {idle_secs}s (limit {idle_kill_secs}s)"),
        TaskResult::Success(_) => unreachable!("Success doesn't trigger retry"),
    }
}

fn compute_backoff(cfg: &RetryConfig, attempt: u32) -> Duration {
    let secs = match cfg.backoff {
        BackoffStrategy::Fixed => cfg.initial_delay,
        BackoffStrategy::Linear => cfg.initial_delay.saturating_mul(attempt as u64),
        BackoffStrategy::Exponential => {
            let exp = (attempt - 1).min(63);
            cfg.initial_delay.saturating_mul(2u64.saturating_pow(exp))
        }
    };
    Duration::from_secs(secs.min(cfg.max_delay))
}

fn execute_attempt(
    task_name: &str,
    mut cmd: std::process::Command,
    opts: &AttemptOpts,
) -> Result<TaskResult, Error> {
    let AttemptOpts {
        capture,
        timeout,
        render_mode,
        idle_warn,
        idle_kill,
    } = *opts;

    // Close stdin: any AI tool (or sub-process like git over SSH) that tries to
    // read interactive input gets EOF and fails fast instead of hanging forever.
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    // Put the child in its own process group so kill signals reach the whole
    // subtree. Without this, signalling the leader (e.g. `sh -c '... sleep N'`)
    // leaves the forked grandchildren alive holding our pipe fds, and the
    // reader threads block on EOF that never comes.
    cmd.process_group(0);

    let mut child = cmd.spawn().map_err(|e| {
        report::status_line(&format!("✗ failed to start task {task_name:?}: {e}"));
        e
    })?;

    let pid = child.id() as i32;
    let activity = Activity::new();
    let killed_for_idle = Arc::new(AtomicBool::new(false));

    // Drop guard ensures the supervisor stops cleanly on any early return.
    let _supervisor = report::spawn_supervisor(
        task_name.to_string(),
        pid,
        Arc::clone(&activity),
        idle_warn,
        idle_kill,
        Arc::clone(&killed_for_idle),
    );

    let stderr_handle = child.stderr.take().map(|stderr| {
        let activity = Arc::clone(&activity);
        std::thread::spawn(move || -> std::io::Result<String> {
            let reader = BufReader::new(stderr);
            let mut accumulated = String::new();
            for line in reader.lines() {
                let line = line?;
                activity.mark_active();
                report::status_line(&line);
                accumulated.push_str(&line);
                accumulated.push('\n');
            }
            Ok(accumulated)
        })
    });

    let stdout_handle = child.stdout.take().map(|stdout| {
        let render_mode = render_mode.clone();
        let want_capture = capture;
        let activity = Arc::clone(&activity);
        std::thread::spawn(move || -> std::io::Result<Option<String>> {
            let mut accumulated = if want_capture {
                Some(String::new())
            } else {
                None
            };
            let stdout_lock = std::io::stdout().lock();
            let mut renderer = match render_mode {
                RenderMode::On(assets) => Some(StreamingRenderer::new(stdout_lock, assets)),
                RenderMode::Off => None,
            };
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let line = line?;
                activity.mark_active();
                if let Some(r) = renderer.as_mut() {
                    r.push_line(&line);
                } else {
                    println!("{line}");
                }
                if let Some(s) = accumulated.as_mut() {
                    s.push_str(&line);
                    s.push('\n');
                }
            }
            if let Some(r) = renderer.as_mut() {
                r.finish();
            }
            // Best-effort flush so any buffered ANSI lands before the next task's banner.
            let _ = std::io::stdout().flush();
            Ok(accumulated)
        })
    });

    let (status, timed_out) = match timeout {
        Some(d) => match child.wait_timeout(d)? {
            Some(s) => (s, false),
            None => {
                // SAFETY: SIGKILL to the whole process group; killing only the
                // leader leaves grandchildren holding our pipe fds open.
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
                let s = child.wait()?;
                (s, true)
            }
        },
        None => (child.wait()?, false),
    };

    let stderr_output = match stderr_handle {
        Some(handle) => handle.join().expect("stderr reader thread panicked")?,
        None => String::new(),
    };

    let stdout_output = match stdout_handle {
        Some(handle) => handle.join().expect("stdout reader thread panicked")?,
        None => None,
    };

    if timed_out {
        return Ok(TaskResult::TimedOut(stderr_tail(&stderr_output, 20)));
    }

    // Check idle-kill BEFORE the signal branch — the supervisor's SIGTERM would
    // otherwise be reported as a generic signal exit.
    if killed_for_idle.load(Ordering::Acquire) {
        return Ok(TaskResult::IdleKilled {
            stderr_tail: stderr_tail(&stderr_output, 20),
            idle_secs: activity.idle_for().as_secs(),
            idle_kill_secs: idle_kill.map(|d| d.as_secs()).unwrap_or(0),
        });
    }

    if status.success() {
        Ok(TaskResult::Success(stdout_output))
    } else {
        let tail = stderr_tail(&stderr_output, 20);
        match status.code() {
            Some(code) => Ok(TaskResult::Failed(code, tail)),
            None => Ok(TaskResult::Signaled(tail)),
        }
    }
}

/// Return the last `n` lines of `s`, trimmed.
fn stderr_tail(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

fn format_command(cmd: &std::process::Command) -> String {
    let program = cmd.get_program().to_string_lossy();
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| shell_quote(&a.to_string_lossy()))
        .collect();

    format!("{program} {}", args.join(" "))
}

fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    let safe = s
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '=' | ':' | ','));
    if safe {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn print_command(
    task_name: &str,
    cmd: &std::process::Command,
    timeout: Option<Duration>,
    retry: Option<&RetryConfig>,
) {
    let mut annotations = Vec::new();
    if let Some(t) = timeout {
        annotations.push(format!("timeout {}s", t.as_secs()));
    }
    if let Some(r) = retry
        && r.attempts > 1
    {
        let backoff = match r.backoff {
            BackoffStrategy::Fixed => "fixed",
            BackoffStrategy::Linear => "linear",
            BackoffStrategy::Exponential => "exponential",
        };
        annotations.push(format!("retry {}x {backoff}", r.attempts));
    }
    let suffix = if annotations.is_empty() {
        String::new()
    } else {
        format!("  ({})", annotations.join(", "))
    };
    println!("[{task_name}] {}{suffix}", format_command(cmd));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::path::Path;

    fn parse_config(toml: &str) -> Config {
        Config::from_str(toml, Path::new("Amakefile")).unwrap()
    }

    #[test]
    fn simple_order() {
        let cfg = parse_config(
            r#"
[tasks.a]
prompt = "A"
[tasks.b]
prompt = "B"
depends = ["a"]
"#,
        );
        let order = resolve_execution_order(&cfg, &["b".into()]).unwrap();
        assert_eq!(order, &["a", "b"]);
    }

    #[test]
    fn diamond_deps() {
        let cfg = parse_config(
            r#"
[tasks.a]
prompt = "A"
[tasks.b]
prompt = "B"
depends = ["a"]
[tasks.c]
prompt = "C"
depends = ["a"]
[tasks.d]
prompt = "D"
depends = ["b", "c"]
"#,
        );
        let order = resolve_execution_order(&cfg, &["d".into()]).unwrap();
        let a_pos = order.iter().position(|x| x == "a").unwrap();
        let b_pos = order.iter().position(|x| x == "b").unwrap();
        let c_pos = order.iter().position(|x| x == "c").unwrap();
        let d_pos = order.iter().position(|x| x == "d").unwrap();
        assert!(a_pos < b_pos);
        assert!(a_pos < c_pos);
        assert!(b_pos < d_pos);
        assert!(c_pos < d_pos);
    }

    #[test]
    fn cycle_detected() {
        let cfg = parse_config(
            r#"
[tasks.a]
prompt = "A"
depends = ["b"]
[tasks.b]
prompt = "B"
depends = ["a"]
"#,
        );
        let result = resolve_execution_order(&cfg, &["a".into()]);
        assert!(matches!(result, Err(Error::DependencyCycle(_))));
    }

    #[test]
    fn unknown_task_error() {
        let cfg = parse_config(
            r#"
[tasks.a]
prompt = "A"
"#,
        );
        let result = resolve_execution_order(&cfg, &["nonexistent".into()]);
        assert!(matches!(result, Err(Error::UnknownTask(_))));
    }

    #[test]
    fn no_deps_single_task() {
        let cfg = parse_config(
            r#"
[tasks.a]
prompt = "A"
"#,
        );
        let order = resolve_execution_order(&cfg, &["a".into()]).unwrap();
        assert_eq!(order, &["a"]);
    }

    #[test]
    fn multiple_targets() {
        let cfg = parse_config(
            r#"
[tasks.a]
prompt = "A"
[tasks.b]
prompt = "B"
"#,
        );
        let order = resolve_execution_order(&cfg, &["a".into(), "b".into()]).unwrap();
        assert!(order.contains(&"a".to_string()));
        assert!(order.contains(&"b".to_string()));
    }

    #[test]
    fn unknown_dep_error() {
        let cfg = parse_config(
            r#"
[tasks.a]
prompt = "A"
depends = ["nonexistent"]
"#,
        );
        let result = resolve_execution_order(&cfg, &["a".into()]);
        assert!(matches!(result, Err(Error::UnknownTask(_))));
    }

    fn retry(backoff: BackoffStrategy, initial: u64, max: u64) -> RetryConfig {
        RetryConfig {
            attempts: 5,
            backoff,
            initial_delay: initial,
            max_delay: max,
            on_timeout: true,
        }
    }

    #[test]
    fn backoff_fixed_is_constant() {
        let cfg = retry(BackoffStrategy::Fixed, 2, 60);
        assert_eq!(compute_backoff(&cfg, 1), Duration::from_secs(2));
        assert_eq!(compute_backoff(&cfg, 4), Duration::from_secs(2));
    }

    #[test]
    fn backoff_linear_scales_with_attempt() {
        let cfg = retry(BackoffStrategy::Linear, 3, 60);
        assert_eq!(compute_backoff(&cfg, 1), Duration::from_secs(3));
        assert_eq!(compute_backoff(&cfg, 2), Duration::from_secs(6));
        assert_eq!(compute_backoff(&cfg, 3), Duration::from_secs(9));
    }

    #[test]
    fn backoff_exponential_doubles() {
        let cfg = retry(BackoffStrategy::Exponential, 1, 60);
        assert_eq!(compute_backoff(&cfg, 1), Duration::from_secs(1));
        assert_eq!(compute_backoff(&cfg, 2), Duration::from_secs(2));
        assert_eq!(compute_backoff(&cfg, 3), Duration::from_secs(4));
        assert_eq!(compute_backoff(&cfg, 4), Duration::from_secs(8));
    }

    #[test]
    fn backoff_caps_at_max_delay() {
        let cfg = retry(BackoffStrategy::Exponential, 1, 5);
        assert_eq!(compute_backoff(&cfg, 1), Duration::from_secs(1));
        assert_eq!(compute_backoff(&cfg, 2), Duration::from_secs(2));
        assert_eq!(compute_backoff(&cfg, 3), Duration::from_secs(4));
        assert_eq!(compute_backoff(&cfg, 4), Duration::from_secs(5));
        assert_eq!(compute_backoff(&cfg, 30), Duration::from_secs(5));
    }
}
