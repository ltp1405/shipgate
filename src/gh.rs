//! GitHub via the `gh` CLI. The PR is the authoritative source of the base ref.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pr {
    pub number: u64,
    pub title: String,
    pub base_ref_name: String,
    pub head_ref_name: String,
    pub head_ref_oid: String,
    pub is_draft: bool,
    pub url: String,
}

fn gh(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("gh")
        .current_dir(dir)
        .args(args)
        .output()
        .with_context(|| format!("failed to run gh {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "gh {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn repo_slug(dir: &Path) -> Result<String> {
    #[derive(Deserialize)]
    struct R {
        #[serde(rename = "nameWithOwner")]
        name_with_owner: String,
    }
    let out = gh(dir, &["repo", "view", "--json", "nameWithOwner"])?;
    Ok(serde_json::from_str::<R>(&out)?.name_with_owner)
}

/// The PR for the current branch, if one exists.
pub fn pr_for_branch(dir: &Path, branch: &str) -> Result<Option<Pr>> {
    let fields = "number,title,baseRefName,headRefName,headRefOid,isDraft,url";
    let out = match gh(dir, &["pr", "view", branch, "--json", fields]) {
        Ok(o) => o,
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("no pull requests found") || msg.contains("no open pull requests") {
                return Ok(None);
            }
            return Err(e);
        }
    };
    Ok(Some(serde_json::from_str(&out)?))
}

pub fn pr_ready(dir: &Path, number: u64) -> Result<()> {
    gh(dir, &["pr", "ready", &number.to_string()])?;
    Ok(())
}

/// The PR's description as it stands right now. Read at submission time rather
/// than when the gate was created: a quiz takes minutes, and whatever was
/// written in the meantime is not ours to discard.
pub fn pr_body(dir: &Path, number: u64) -> Result<String> {
    #[derive(Deserialize)]
    struct B {
        body: String,
    }
    let out = gh(dir, &["pr", "view", &number.to_string(), "--json", "body"])?;
    Ok(serde_json::from_str::<B>(&out)?.body)
}

pub fn pr_set_body(dir: &Path, number: u64, body_file: &Path) -> Result<()> {
    gh(
        dir,
        &[
            "pr",
            "edit",
            &number.to_string(),
            "--body-file",
            &body_file.to_string_lossy(),
        ],
    )?;
    Ok(())
}

/// Commit subjects on the PR, used as the description fallback when there is
/// no justification answer to build "What this changes" from.
pub fn pr_commits(dir: &Path, number: u64) -> Result<Vec<String>> {
    #[derive(Deserialize)]
    struct C {
        #[serde(rename = "messageHeadline")]
        headline: String,
    }
    #[derive(Deserialize)]
    struct R {
        commits: Vec<C>,
    }
    let out = gh(dir, &["pr", "view", &number.to_string(), "--json", "commits"])?;
    let r: R = serde_json::from_str(&out)?;
    Ok(r.commits.into_iter().map(|c| c.headline).collect())
}

/// Open PRs, for `shipgate status`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrBrief {
    pub number: u64,
    pub title: String,
    pub is_draft: bool,
    pub head_ref_name: String,
    pub updated_at: String,
}

pub fn my_open_prs(dir: &Path) -> Result<Vec<PrBrief>> {
    let out = gh(
        dir,
        &[
            "pr",
            "list",
            "--author",
            "@me",
            "--state",
            "open",
            "--json",
            "number,title,isDraft,headRefName,updatedAt",
        ],
    )?;
    Ok(serde_json::from_str(&out)?)
}
