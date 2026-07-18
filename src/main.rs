mod adapter;
mod config;
mod editor;
mod error;
mod profile;
mod render;
mod report;
mod runner;
mod sandbox;
mod template;

use clap::{Parser, Subcommand};
use config::Config;
use error::Error;
use profile::ProfileSet;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "amake", version, about = "A task runner for AI CLI tools")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run one or more tasks
    Run {
        /// Task names to execute (dependencies auto-included)
        #[arg(required = true)]
        tasks: Vec<String>,

        /// Show resolved commands without executing
        #[arg(long)]
        dry_run: bool,

        /// Continue on failure
        #[arg(short = 'k', long)]
        keep_going: bool,

        /// Set a variable (repeatable), e.g. --var key=value
        #[arg(long = "var", value_name = "KEY=VALUE")]
        vars: Vec<String>,

        /// Open $EDITOR to input a variable value (repeatable), e.g. --edit-var description
        #[arg(long = "edit-var", value_name = "NAME")]
        edit_vars: Vec<String>,

        /// Path to Amakefile (skip auto-discovery)
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,

        /// Force-enable clampdown sandbox for all tasks
        #[arg(long)]
        sandbox: bool,

        /// Disable sandbox for all tasks (overrides config)
        #[arg(long)]
        no_sandbox: bool,

        /// Disable syntax-highlighted markdown rendering of task output
        #[arg(long)]
        no_format: bool,
    },

    /// List all tasks in the Amakefile
    List {
        /// Path to Amakefile (skip auto-discovery)
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
    },

    /// List built-in adapters
    Adapters,

    /// Manage profiles
    Profile {
        #[command(subcommand)]
        action: ProfileAction,
    },
}

#[derive(Subcommand)]
enum ProfileAction {
    /// List all resolved profiles for the current directory, with availability markers
    List,

    /// Write built-in profiles to ~/.config/amake/config.toml (create if absent)
    Init,

    /// Show which profile would be selected for a given task
    Which {
        /// Task name
        task: String,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Error> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Run {
            tasks,
            dry_run,
            keep_going,
            vars: cli_vars,
            edit_vars,
            file,
            sandbox,
            no_sandbox,
            no_format,
        } => {
            let config = load_config(file)?;
            let mut vars = config.vars.clone();
            for (k, v) in parse_vars(&cli_vars)? {
                vars.insert(k, v);
            }

            for name in &edit_vars {
                report::status_line(&format!("✎ opening editor for variable: {name}"));
                let value = editor::edit_variable(name)?;
                vars.insert(name.clone(), value);
            }

            // Load profiles (auto-generate home config on first run).
            let cwd = std::env::current_dir()?;
            let _ = ProfileSet::auto_generate_home_config();
            let (profile_set, _) = ProfileSet::resolve_layered(&cwd)?;

            runner::run(
                &config,
                &tasks,
                &runner::RunOptions {
                    dry_run,
                    keep_going,
                    force_sandbox: sandbox,
                    no_sandbox,
                    no_format,
                    vars,
                    profile_set,
                },
            )
        }

        Commands::List { file } => {
            let config = load_config(file)?;
            list_tasks(&config);
            Ok(())
        }

        Commands::Adapters => {
            let registry = adapter::AdapterRegistry::new();
            for name in registry.builtin_names() {
                println!("{name}");
            }
            Ok(())
        }

        Commands::Profile { action } => handle_profile(action),
    }
}

fn load_config(file: Option<PathBuf>) -> Result<Config, Error> {
    let path = match file {
        Some(p) => p,
        None => config::find_amakefile()?,
    };
    Config::load(&path)
}

fn parse_vars(vars: &[String]) -> Result<BTreeMap<String, String>, Error> {
    vars.iter()
        .map(|v| {
            v.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .ok_or_else(|| Error::UnresolvedVariable {
                    task: "<cli>".into(),
                    variable: v.clone(),
                    hint: "variables must be in KEY=VALUE format".into(),
                })
        })
        .collect()
}

fn list_tasks(config: &Config) {
    if config.tasks.is_empty() {
        println!("No tasks defined.");
        return;
    }

    let max_name = config.tasks.keys().map(|n| n.len()).max().unwrap_or(0);

    for (name, task) in &config.tasks {
        let tool = task
            .tool
            .as_deref()
            .or(config.defaults.tool.as_deref())
            .unwrap_or("(none)");

        let first_line = task
            .prompt
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim();

        let truncated = if first_line.len() > 60 {
            format!("{}...", &first_line[..57])
        } else {
            first_line.to_string()
        };

        println!("  {name:<max_name$}  [{tool}]  {truncated}");
    }
}

fn handle_profile(action: ProfileAction) -> Result<(), Error> {
    match action {
        ProfileAction::List => {
            let cwd = std::env::current_dir()?;
            let (set, project_path) = ProfileSet::resolve_layered(&cwd)?;

            println!("Profiles resolved for {}", cwd.display());
            println!("  Home config:   {}", ProfileSet::home_config_path().display());
            if let Some(ref p) = project_path {
                println!("  Project config: {}", p.display());
            }
            println!();

            for (name, prof) in set.iter() {
                let available = if ProfileSet::tool_is_available(prof) {
                    "✓"
                } else if prof.tool.is_some() {
                    "✗"
                } else {
                    "?"
                };

                let tool_str = prof.tool.as_deref().unwrap_or("(none)");
                let model_str = prof.model.as_deref().unwrap_or("(any)");
                println!("  {available} {name:<20}  tool={tool_str:<15} model={model_str}");
            }

            Ok(())
        }

        ProfileAction::Init => {
            let created = ProfileSet::auto_generate_home_config()?;
            if created {
                println!(
                    "Created {}",
                    ProfileSet::home_config_path().display()
                );
            } else {
                println!(
                    "{} already exists",
                    ProfileSet::home_config_path().display()
                );
            }
            Ok(())
        }

        ProfileAction::Which { task } => {
            // Load the config to get the task's profile field
            let config = match load_config(None) {
                Ok(c) => c,
                Err(_) => {
                    // If no Amakefile, just show resolution without task context
                    let cwd = std::env::current_dir()?;
                    let (set, _) = ProfileSet::resolve_layered(&cwd)?;
                    let names: Vec<String> = task.split(',').map(|s| s.trim().to_string()).collect();
                    let (resolved, diag) = set.resolve_chain(&names);
                    print_resolution(&names, &resolved, &diag);
                    return Ok(());
                }
            };

            if !config.tasks.contains_key(&task) {
                return Err(Error::UnknownTask(task));
            }

            let cwd = std::env::current_dir()?;
            let (set, _) = ProfileSet::resolve_layered(&cwd)?;

            let task_profile_names = &config.tasks[&task].profile;
            let (resolved, diag) = set.resolve_chain(task_profile_names);
            print_resolution(task_profile_names, &resolved, &diag);
            Ok(())
        }
    }
}

fn print_resolution(
    names: &[String],
    resolved: &Option<(&str, &profile::Profile)>,
    diag: &[String],
) {
    match resolved {
        Some((name, prof)) => {
            let names_str = if names.is_empty() {
                "(default)".to_string()
            } else {
                names.join(" → ")
            };
            println!("Profile chain: {names_str}");
            println!("Resolved:      {name}");
            println!("  tool:  {}", prof.tool.as_deref().unwrap_or("(none)"));
            println!("  model: {}", prof.model.as_deref().unwrap_or("(any)"));
        }
        None => {
            eprintln!("No available profile found.");
            for d in diag {
                eprintln!("  warning: {d}");
            }
        }
    }
}
