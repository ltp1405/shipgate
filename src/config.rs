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
}

#[derive(Debug, Default, Deserialize)]
pub struct RepoConfig {
    pub base: Option<String>,
}

pub fn load() -> Config {
    let Some(dirs) = directories::ProjectDirs::from("", "", "shipgate") else {
        return Config::default();
    };
    let path = dirs.config_dir().join("config.toml");
    let Ok(text) = std::fs::read_to_string(path) else {
        return Config::default();
    };
    toml::from_str(&text).unwrap_or_default()
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
