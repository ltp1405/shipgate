//! §5 — base branch resolution. `@{upstream}` → origin/main does not resolve
//! in every workflow: features branch from develop, hotfixes from master.

use anyhow::Result;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub repo: HashMap<String, RepoConfig>,
    #[serde(default)]
    pub models: Models,
    /// Repositories the dashboard watches for PRs you have not gated yet.
    /// Gates already carry their own path, so this only matters for discovery.
    #[serde(default)]
    pub watch: Vec<Watch>,
}

#[derive(Debug, Deserialize)]
pub struct Watch {
    pub path: String,
}

impl Watch {
    /// Expand a leading `~`, since a config file is written by hand.
    pub fn expanded(&self) -> std::path::PathBuf {
        match self.path.strip_prefix("~/") {
            Some(rest) => directories::UserDirs::new()
                .map(|d| d.home_dir().join(rest))
                .unwrap_or_else(|| std::path::PathBuf::from(&self.path)),
            None => std::path::PathBuf::from(&self.path),
        }
    }
}

/// Which model writes the questions and which grades them.
///
/// They must differ. One model writing the question, the reference *and* the
/// grade lets a wrong premise through unchallenged (§8); two decorrelate it.
/// Generation is the larger share of the bill, so it is the first thing to
/// lower when usage bites.
#[derive(Debug, Deserialize)]
pub struct Models {
    #[serde(default = "default_generate")]
    pub generate: String,
    #[serde(default = "default_judge")]
    pub judge: String,
}

fn default_generate() -> String {
    "sonnet".into()
}

fn default_judge() -> String {
    "haiku".into()
}

impl Default for Models {
    fn default() -> Self {
        Self {
            generate: default_generate(),
            judge: default_judge(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct RepoConfig {
    pub base: Option<String>,
}

/// Where the config lives.
///
/// `directories` returns the platform-native directory, which on macOS is
/// `~/Library/Application Support/shipgate` — not where anyone looks for a
/// command-line tool's config. XDG first, then `~/.config`, then the
/// platform default, and an existing file wins over a merely-possible one.
pub fn config_path() -> Option<std::path::PathBuf> {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();

    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.trim().is_empty() {
            candidates.push(std::path::PathBuf::from(xdg).join("shipgate/config.toml"));
        }
    }
    if let Some(dirs) = directories::UserDirs::new() {
        candidates.push(dirs.home_dir().join(".config/shipgate/config.toml"));
    }
    if let Some(dirs) = directories::ProjectDirs::from("", "", "shipgate") {
        candidates.push(dirs.config_dir().join("config.toml"));
    }

    candidates
        .iter()
        .find(|p| p.exists())
        .cloned()
        .or_else(|| candidates.into_iter().next())
}

pub fn load() -> Config {
    let Some(path) = config_path() else {
        return Config::default();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Config::default();
    };
    match toml::from_str(&text) {
        Ok(c) => c,
        Err(e) => {
            // Silently falling back to defaults would hide a typo in a file the
            // user wrote by hand.
            eprintln!("warning: {} is not valid TOML: {e}", path.display());
            Config::default()
        }
    }
}

/// Resolution order: the PR's own base (authoritative, handled by the caller),
/// then per-repo config, then develop → master → main, then the remote HEAD.
pub fn fallback_base(dir: &Path, repo_slug: &str, cfg: &Config) -> Result<String> {
    let remote = crate::git::default_remote(dir)?;

    if let Some(base) = cfg.repo.get(repo_slug).and_then(|r| r.base.as_deref()) {
        let r = format!("{remote}/{base}");
        if crate::git::ref_exists(dir, &r) {
            return Ok(r);
        }
    }

    for candidate in ["develop", "master", "main"] {
        let r = format!("{remote}/{candidate}");
        if crate::git::ref_exists(dir, &r) {
            return Ok(r);
        }
    }

    let head = crate::git::git(dir, &["symbolic-ref", &format!("refs/remotes/{remote}/HEAD")])?;
    Ok(head.trim().trim_start_matches("refs/remotes/").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_use_two_different_models() {
        let m = Models::default();
        assert_eq!(m.generate, "sonnet");
        assert_eq!(m.judge, "haiku");
        // §8 rests on these differing: one model writing the question, the
        // reference and the grade lets a wrong premise through unchallenged.
        assert_ne!(m.generate, m.judge);
    }

    #[test]
    fn an_empty_config_still_yields_models() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.models.generate, "sonnet");
    }

    #[test]
    fn either_model_can_be_overridden_alone() {
        let c: Config = toml::from_str("[models]\ngenerate = \"opus\"\n").unwrap();
        assert_eq!(c.models.generate, "opus");
        assert_eq!(c.models.judge, "haiku", "judge should keep its default");
    }

    #[test]
    fn repo_bases_and_models_coexist() {
        let c: Config = toml::from_str(
            "[models]\njudge = \"sonnet\"\n\n[repo.\"PostCo/project-tapir\"]\nbase = \"develop\"\n",
        )
        .unwrap();
        assert_eq!(c.models.judge, "sonnet");
        assert_eq!(
            c.repo["PostCo/project-tapir"].base.as_deref(),
            Some("develop")
        );
    }
}
