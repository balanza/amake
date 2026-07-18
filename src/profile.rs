use crate::error::Error;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A single profile definition: user-level overrides for tool, model, and other task settings.
///
/// Profiles sit between a task's explicit fields and the Amakefile's `[defaults]`:
/// `task > resolved profile > Amakefile defaults`
#[derive(Debug, Clone, Default)]
pub struct Profile {
    /// The adapter/tool binary to use (e.g. "claude", "aider")
    pub tool: Option<String>,
    /// The model to pass via --model (e.g. "sonnet", "opus")
    pub model: Option<String>,
    /// Tool-specific "don't ask" flag
    pub auto_approve: Option<bool>,
    /// Extra CLI arguments appended to the tool invocation
    pub extra_args: Option<Vec<String>>,
    /// Per-task timeout in seconds
    pub timeout: Option<u64>,
    /// Seconds of silence before a warning is emitted
    pub idle_warn: Option<u64>,
    /// Seconds of silence before the child is killed
    pub idle_kill: Option<u64>,
}

impl Profile {
    pub fn new(tool: &str, model: &str) -> Self {
        Self {
            tool: Some(tool.to_string()),
            model: Some(model.to_string()),
            ..Default::default()
        }
    }
}

/// Deserialization-only mirror of `Profile`, matching the TOML table shape.
#[derive(serde::Deserialize)]
struct RawProfile {
    tool: Option<String>,
    model: Option<String>,
    auto_approve: Option<bool>,
    #[serde(default)]
    extra_args: Option<Vec<String>>,
    timeout: Option<u64>,
    idle_warn: Option<u64>,
    idle_kill: Option<u64>,
}

impl From<RawProfile> for Profile {
    fn from(r: RawProfile) -> Self {
        Self {
            tool: r.tool,
            model: r.model,
            auto_approve: r.auto_approve,
            extra_args: r.extra_args.filter(|v| !v.is_empty()),
            timeout: r.timeout,
            idle_warn: r.idle_warn,
            idle_kill: r.idle_kill,
        }
    }
}

/// A collection of named profiles, with support for layered loading,
/// merging, availability checks, and resolution.
///
/// Layering (lowest → highest priority):
///   1. Built-in defaults (`amake-large`, `amake-medium`, `amake-small`)
///   2. User home config  (`~/.config/amake/config.toml`)
///   3. Project `.amakerc` (first found walking cwd → home)
#[derive(Debug, Clone)]
pub struct ProfileSet {
    profiles: BTreeMap<String, Profile>,
}

impl Default for ProfileSet {
    fn default() -> Self {
        Self::builtins()
    }
}

#[allow(dead_code)]
impl ProfileSet {
    /// Create an empty set (no built-in profiles).
    pub fn empty() -> Self {
        Self {
            profiles: BTreeMap::new(),
        }
    }

    /// Build the default set with the three built-in profiles.
    ///
    /// These are hard-coded as a first iteration. A future enhancement will
    /// heuristically infer defaults from the host machine.
    pub fn builtins() -> Self {
        let mut set = Self::empty();
        set.insert("amake-large".into(), Profile::new("claude-code", "opus"));
        set.insert(
            "amake-medium".into(),
            Profile::new("claude-code", "sonnet"),
        );
        set.insert("amake-small".into(), Profile::new("claude-code", "haiku"));
        set
    }

    // ── accessors ──

    pub fn get(&self, name: &str) -> Option<&Profile> {
        self.profiles.get(name)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.profiles.contains_key(name)
    }

    pub fn insert(&mut self, name: String, profile: Profile) {
        self.profiles.insert(name, profile);
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Profile)> {
        self.profiles.iter()
    }

    pub fn names(&self) -> Vec<&str> {
        self.profiles.keys().map(|s| s.as_str()).collect()
    }

    // ── building / merging ──

    /// Merge another `ProfileSet` into this one.
    ///
    /// The other set's entries override same-named entries in `self`
    /// (i.e. "last wins"). This is used when layering higher-priority
    /// configs (home overrides built-ins, project overrides home).
    fn merge(&mut self, other: ProfileSet) {
        for (name, profile) in other.profiles {
            self.profiles.insert(name, profile);
        }
    }

    // ── loading from disk ──

    /// Load profiles from a TOML file.
    pub fn load_from_file(path: &Path) -> Result<Self, Error> {
        let contents =
            std::fs::read_to_string(path).map_err(|e| Error::ProfileConfigRead {
                path: path.to_path_buf(),
                source: e,
            })?;
        Self::load_from_str(&contents, path)
    }

    /// Parse a TOML string into a `ProfileSet`.
    pub fn load_from_str(s: &str, path: &Path) -> Result<Self, Error> {
        #[derive(serde::Deserialize)]
        struct RawConfig {
            #[serde(default)]
            profile: BTreeMap<String, RawProfile>,
        }

        let raw: RawConfig = toml::from_str(s).map_err(|e| Error::ProfileConfigParse {
            path: path.to_path_buf(),
            source: e,
        })?;

        let mut set = Self::empty();
        for (name, raw_profile) in raw.profile {
            set.insert(name, Profile::from(raw_profile));
        }
        Ok(set)
    }

    /// Resolve the full layered `ProfileSet` for a given working directory.
    ///
    /// Returns the merged result of:
    /// 1. Built-in defaults
    /// 2. Home config (`~/.config/amake/config.toml`)
    /// 3. Project `.amakerc` (first found walking from `cwd` toward home)
    ///
    /// Also returns the path of the project-level config if it was found.
    pub fn resolve_layered(cwd: &Path) -> Result<(Self, Option<PathBuf>), Error> {
        // 1. Built-in defaults (lowest priority)
        let mut merged = Self::builtins();

        // 2. Home config
        let home_path = Self::home_config_path();
        if home_path.exists() {
            let home_profiles = Self::load_from_file(&home_path)?;
            merged.merge(home_profiles);
        }

        // 3. Project-level .amakerc (highest priority)
        let project_path = Self::find_project_config(cwd);
        if let Some(ref path) = project_path {
            let project_profiles = Self::load_from_file(path)?;
            merged.merge(project_profiles);
        }

        Ok((merged, project_path))
    }

    /// Walk from `cwd` up to the home directory, returning the first `.amakerc`
    /// found (stopping before home so the home config stays separate).
    fn find_project_config(cwd: &Path) -> Option<PathBuf> {
        let home = home_dir()?;
        let mut dir = Some(cwd);

        while let Some(d) = dir {
            // Don't search the home directory itself — the home config is
            // always loaded from the XDG path.
            if d == home {
                break;
            }

            let candidate = d.join(".amakerc");
            if candidate.is_file() {
                return Some(candidate);
            }

            dir = d.parent();
        }

        None
    }

    /// Return the canonical path to the user-level config file.
    pub fn home_config_path() -> PathBuf {
        let home = home_dir().unwrap_or_else(|| PathBuf::from("~"));
        home.join(".config")
            .join("amake")
            .join("config.toml")
    }

    /// Auto-generate the home config file if it does not already exist.
    ///
    /// Returns `true` if the file was created, `false` if it already existed.
    pub fn auto_generate_home_config() -> Result<bool, Error> {
        let path = Self::home_config_path();
        if path.exists() {
            return Ok(false);
        }

        let content = r#"# Amake user profiles
#
# This file was auto-generated by amake. Edit it to customise your default
# agents and models for different profile tiers.
#
# Profiles defined here can be overridden by a project-level .amakerc file
# placed in any directory between your project root and the current working
# directory.

[profile.amake-large]
tool = "claude-code"
model = "opus"

[profile.amake-medium]
tool = "claude-code"
model = "sonnet"

[profile.amake-small]
tool = "claude-code"
model = "haiku"
"#;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(Error::Io)?;
        }

        std::fs::write(&path, content).map_err(|e| Error::ProfileConfigRead {
            path: path.clone(),
            source: e,
        })?;

        Ok(true)
    }

    // ── availability & resolution ──

    /// Check whether a profile's `tool` binary is present on `PATH`.
    pub fn tool_is_available(profile: &Profile) -> bool {
        match profile.tool.as_deref() {
            Some(tool) => tool_on_path(tool_binary_name(tool)),
            None => false,
        }
    }

    /// Resolve the first usable profile from a chain of profile names.
    ///
    /// Each name in `profile_names` is looked up in the set. If the profile
    /// exists and its tool is on `PATH`, it is returned immediately.
    /// If none of the names resolves, the built-in fallback `amake-medium` is
    /// tried.  If that also fails, `None` is returned (caller should warn).
    pub fn resolve<'a>(
        &'a self,
        profile_names: &'a [String],
    ) -> Option<(&'a str, &'a Profile)> {
        // Try the explicitly named profiles, in order.
        for name in profile_names {
            if let Some(profile) = self.get(name)
                && Self::tool_is_available(profile)
            {
                return Some((name, profile));
            }
        }

        // Fall back to amake-medium (the universal default).
        if let Some(profile) = self.get("amake-medium")
            && Self::tool_is_available(profile)
        {
            return Some(("amake-medium", profile));
        }

        None
    }

    /// Resolve with diagnostics: returns the resolved profile (if any) and a list
    /// of human-readable reasons why each candidate was skipped.
    ///
    /// The caller can use the `diag` list to emit a warning when the chain is
    /// exhausted.
    pub fn resolve_chain<'a>(
        &'a self,
        profile_names: &'a [String],
    ) -> (Option<(&'a str, &'a Profile)>, Vec<String>) {
        let mut diag: Vec<String> = Vec::new();

        // Try the explicitly named profiles, in order.
        for name in profile_names {
            match self.get(name) {
                None => {
                    diag.push(format!(
                        "profile {name:?} is not defined — skipping"
                    ));
                }
                Some(profile) => {
                    match &profile.tool {
                        None => {
                            diag.push(format!(
                                "profile {name:?} has no tool configured — skipping"
                            ));
                        }
                        Some(tool) if !tool_on_path(tool_binary_name(tool)) => {
                            let binary = tool_binary_name(tool);
                            let extra = if binary != tool {
                                format!(" (binary: {binary:?})")
                            } else {
                                String::new()
                            };
                            diag.push(format!(
                                "profile {name:?} requires {tool:?}{} which is not installed — skipping",
                                extra
                            ));
                        }
                        Some(_) => {
                            // Found a usable profile.
                            return (Some((name, profile)), diag);
                        }
                    }
                }
            }
        }

        // Fall back to amake-medium (the universal default).
        match self.get("amake-medium") {
            None => {
                diag.push("fallback profile amake-medium is not defined — no profile available".into());
            }
            Some(profile) => {
                match &profile.tool {
                    None => {
                        diag.push("fallback profile amake-medium has no tool configured — no profile available".into());
                    }
                    Some(tool) if !tool_on_path(tool_binary_name(tool)) => {
                        let binary = tool_binary_name(tool);
                        let extra = if binary != tool {
                            format!(" (binary: {binary:?})")
                        } else {
                            String::new()
                        };
                        diag.push(format!(
                            "fallback profile amake-medium requires {tool:?}{} which is not installed — no profile available",
                            extra
                        ));
                    }
                    Some(_) => {
                        return (Some(("amake-medium", profile)), diag);
                    }
                }
            }
        }

        (None, diag)
    }
}

// ── helpers ──

/// Return the home directory, preferring `$HOME`.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Map an adapter name to the actual binary filename on `$PATH`.
///
/// Several built-in adapters have binaries named differently from the
/// adapter name (e.g. `"claude-code"` → `"claude"`, `"copilot"` → `"gh"`).
/// Unknown names are assumed to be the binary name directly.
fn tool_binary_name(tool: &str) -> &str {
    match tool {
        "claude-code" => "claude",
        "copilot" => "gh",
        other => other,
    }
}

/// Check whether a bare binary name exists somewhere on `$PATH`.
fn tool_on_path(tool: &str) -> bool {
    std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(tool))
                .find(|path| path.is_file())
        })
        .is_some()
}

// ── tests ──

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_are_present() {
        let set = ProfileSet::builtins();
        assert!(set.contains("amake-large"));
        assert!(set.contains("amake-medium"));
        assert!(set.contains("amake-small"));
    }

    #[test]
    fn builtins_have_correct_tool_and_model() {
        let set = ProfileSet::builtins();
        let large = set.get("amake-large").unwrap();
        assert_eq!(large.tool.as_deref(), Some("claude-code"));
        assert_eq!(large.model.as_deref(), Some("opus"));

        let medium = set.get("amake-medium").unwrap();
        assert_eq!(medium.tool.as_deref(), Some("claude-code"));
        assert_eq!(medium.model.as_deref(), Some("sonnet"));

        let small = set.get("amake-small").unwrap();
        assert_eq!(small.tool.as_deref(), Some("claude-code"));
        assert_eq!(small.model.as_deref(), Some("haiku"));
    }

    #[test]
    fn load_from_toml_parses_profile() {
        let toml = r#"
[profile.fast]
tool = "claude"
model = "sonnet"
auto_approve = true
extra_args = ["--verbose"]
"#;
        let set = ProfileSet::load_from_str(toml, Path::new("test.toml")).unwrap();
        let p = set.get("fast").unwrap();
        assert_eq!(p.tool.as_deref(), Some("claude"));
        assert_eq!(p.model.as_deref(), Some("sonnet"));
        assert_eq!(p.auto_approve, Some(true));
        assert_eq!(
            p.extra_args.as_deref(),
            Some(vec!["--verbose".to_string()].as_slice())
        );
    }

    #[test]
    fn load_from_toml_ignores_other_tables() {
        let toml = r#"
[defaults]
tool = "echo"

[tasks.hello]
prompt = "Hello"

[profile.custom]
tool = "aider"
"#;
        let set = ProfileSet::load_from_str(toml, Path::new("test.toml")).unwrap();
        assert_eq!(set.names(), vec!["custom"]);
    }

    #[test]
    fn empty_profile_set_has_no_profiles() {
        let set = ProfileSet::empty();
        assert!(set.names().is_empty());
    }

    #[test]
    fn merge_lets_higher_priority_win() {
        let mut base = ProfileSet::empty();
        base.insert("a".into(), Profile::new("tool1", "model1"));

        let mut overlay = ProfileSet::empty();
        overlay.insert("a".into(), Profile::new("tool-override", "model-override"));
        overlay.insert("b".into(), Profile::new("tool2", "model2"));

        base.merge(overlay);
        // With merge (insert), the last value wins (higher priority).
        assert_eq!(base.get("a").unwrap().tool.as_deref(), Some("tool-override"));
        assert_eq!(base.get("b").unwrap().tool.as_deref(), Some("tool2"));
    }

    #[test]
    fn merge_with_replace_semantics() {
        let mut base = ProfileSet::empty();
        base.insert("a".into(), Profile::new("tool1", "model1"));

        let mut overlay = ProfileSet::empty();
        overlay.insert("a".into(), Profile::new("override-tool", "override-model"));

        base.merge(overlay);
        assert_eq!(
            base.get("a").unwrap().tool.as_deref(),
            Some("override-tool")
        );
    }

    #[test]
    fn resolve_skips_missing_tool_and_falls_back() {
        // Build a set where "test-profile" references a tool that almost
        // certainly doesn't exist, and there's a fallback using "echo".
        let mut set = ProfileSet::empty();
        set.insert(
            "test-profile".into(),
            Profile {
                tool: Some("this-tool-does-not-exist-hopefully".into()),
                ..Default::default()
            },
        );
        set.insert(
            "amake-medium".into(),
            Profile {
                tool: Some("echo".into()),
                ..Default::default()
            },
        );

        let names: Vec<String> = vec!["test-profile".into()];
        let resolved = set.resolve(&names);

        // The nonexistent profile is skipped, amake-medium (echo) is used.
        assert!(resolved.is_some());
        assert_eq!(resolved.unwrap().0, "amake-medium");
    }

    #[test]
    fn resolve_returns_first_available() {
        let mut set = ProfileSet::empty();
        set.insert(
            "first".into(),
            Profile {
                tool: Some("echo".into()),
                model: Some("first-model".into()),
                ..Default::default()
            },
        );
        set.insert(
            "second".into(),
            Profile {
                tool: Some("echo".into()),
                model: Some("second-model".into()),
                ..Default::default()
            },
        );

        let names: Vec<String> = vec!["first".into(), "second".into()];
        let resolved = set.resolve(&names);
        assert!(resolved.is_some());
        assert_eq!(resolved.unwrap().0, "first");
    }

    #[test]
    fn resolve_unknown_name_tries_fallback() {
        // Override amake-medium to use echo (which is always available).
        let mut set = ProfileSet::empty();
        set.insert(
            "amake-medium".into(),
            Profile {
                tool: Some("echo".into()),
                ..Default::default()
            },
        );
        let names: Vec<String> = vec!["unknown-profile".into()];
        let resolved = set.resolve(&names);
        assert!(resolved.is_some());
        assert_eq!(resolved.unwrap().0, "amake-medium");
    }

    #[test]
    fn home_config_path_is_correct() {
        let path = ProfileSet::home_config_path();
        assert!(
            path.ends_with(".config/amake/config.toml"),
            "unexpected path: {}",
            path.display()
        );
    }

    #[test]
    fn tool_on_path_works() {
        // Most environments have `sh` or `echo` on PATH.
        assert!(tool_on_path("echo"));
        // A made-up binary should not be found.
        assert!(!tool_on_path("this-binary-almost-certainly-does-not-exist-12345"));
    }

    #[test]
    fn resolve_chain_provides_diagnostics() {
        let mut set = ProfileSet::empty();
        set.insert(
            "real".into(),
            Profile {
                tool: Some("echo".into()),
                ..Default::default()
            },
        );

        let names: Vec<String> = vec!["nonexistent".into(), "real".into()];
        let (resolved, diag) = set.resolve_chain(&names);
        assert_eq!(resolved.unwrap().0, "real");
        assert!(
            diag[0].contains("nonexistent"),
            "diag should mention skipped profile"
        );
    }

    #[test]
    fn resolve_chain_exhausted_returns_none_with_reasons() {
        let set = ProfileSet::empty();
        let names: Vec<String> = vec!["nope".into()];
        let (resolved, diag) = set.resolve_chain(&names);
        assert!(resolved.is_none());
        assert!(!diag.is_empty(), "should have diagnostic messages");
    }

    /// Helper: run tests that modify HOME sequentially to avoid races.
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_temp_home(f: impl FnOnce(&std::path::Path)) {
        let _guard = HOME_LOCK.lock().unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let fake_home = tmp.path().join("home");
        std::fs::create_dir_all(&fake_home).unwrap();
        let old_home = std::env::var_os("HOME");
        // SAFETY: test-only, serialized by HOME_LOCK.
        unsafe { std::env::set_var("HOME", &fake_home); }
        f(&fake_home);
        // SAFETY: test-only, serialized by HOME_LOCK.
        match old_home {
            Some(v) => unsafe { std::env::set_var("HOME", v); },
            None => unsafe { std::env::remove_var("HOME"); },
        }
        drop(tmp);
    }

    #[test]
    fn auto_generate_creates_file() {
        with_temp_home(|_| {
            let path = ProfileSet::home_config_path();
            assert!(!path.exists(), "path should not exist before: {}", path.display());
            let created = ProfileSet::auto_generate_home_config().unwrap();
            assert!(created, "first call should create");
            assert!(path.exists(), "path should exist after first call: {}", path.display());

            // Calling again should return false (already exists).
            let created2 = ProfileSet::auto_generate_home_config().unwrap();
            assert!(!created2, "second call should not create");

            // Verify the content parses as valid profiles.
            let loaded = ProfileSet::load_from_file(&path).unwrap();
            assert!(loaded.contains("amake-large"));
            assert!(loaded.contains("amake-medium"));
            assert!(loaded.contains("amake-small"));
        });
    }

    #[test]
    fn layered_resolution_includes_builtins() {
        with_temp_home(|fake_home| {
            let (set, project_path) = ProfileSet::resolve_layered(fake_home).unwrap();
            assert!(
                project_path.is_none(),
                "expected no project config, got: {:?}",
                project_path
            );
            assert!(set.contains("amake-large"));
            assert!(set.contains("amake-medium"));
            assert!(set.contains("amake-small"));
        });
    }
}
