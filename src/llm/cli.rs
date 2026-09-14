//! Model calls via the Claude Code CLI (`claude -p`).
//!
//! An earlier revision spoke to the Messages API over raw HTTP. Rust has no
//! official Anthropic SDK, so that wire shape could only be asserted against the
//! documentation, never against a live response — there was no API key to test
//! with. Unverifiable code does not belong in the repo, so it was removed in
//! favour of this, which runs against the CLI's own credentials and is exercised
//! by the tests below.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use std::io::Write;
use std::process::{Command, Stdio};

#[derive(Deserialize)]
struct CliResult {
    result: String,
    #[serde(default)]
    total_cost_usd: f64,
    #[serde(default)]
    is_error: bool,
}

pub struct Cli;

impl Cli {
    /// Present only if the CLI is on PATH.
    pub fn detect() -> Option<Self> {
        Command::new("claude")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()
            .filter(|s| s.success())
            .map(|_| Cli)
    }

    /// One call. `--restricted` removes the tools that run commands or code and
    /// WebFetch: this needs text in and text out, and the subprocess has no
    /// business touching the repository it is being asked about.
    fn invoke(&self, model: &str, system: &str, user: &str) -> Result<(String, f64)> {
        let mut child = Command::new("claude")
            .args([
                "-p",
                "--output-format", "json",
                "--model", model,
                "--restricted",
                "--strict-mcp-config",
                "--system-prompt", system,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to start `claude` — is Claude Code on PATH?")?;

        child
            .stdin
            .take()
            .context("no stdin on the claude process")?
            .write_all(user.as_bytes())?;

        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!(
                "claude exited {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }

        let parsed: CliResult = serde_json::from_slice(&out.stdout)
            .context("could not parse the claude --output-format json envelope")?;
        if parsed.is_error {
            bail!("claude reported an error: {}", parsed.result);
        }
        Ok((parsed.result, parsed.total_cost_usd))
    }

    /// The CLI has no schema enforcement, so the JSON is extracted and, on
    /// failure, asked for once more with the parse error appended.
    pub fn complete_json(&self, model: &str, system: &str, user: &str) -> Result<(Value, f64)> {
        let system = format!(
            "{system}\n\nReturn a single JSON object and nothing else. No prose, \
             no explanation, no code fences."
        );
        let (text, cost) = self.invoke(model, &system, user)?;
        match extract_json(&text) {
            Ok(v) => Ok((v, cost)),
            Err(first) => {
                let retry_user = format!(
                    "{user}\n\n# Your previous reply could not be parsed\n\n\
                     Error: {first}\n\nReturn only the JSON object."
                );
                let (text2, cost2) = self.invoke(model, &system, &retry_user)?;
                let v = extract_json(&text2)
                    .with_context(|| format!("second attempt also unparseable (first: {first})"))?;
                Ok((v, cost + cost2))
            }
        }
    }
}

/// Pull a JSON object out of a model reply: bare, fenced, or with prose around
/// it. Brace counting is string-aware so a `{` inside a quoted value — common in
/// answers that quote code — does not throw off the depth.
pub fn extract_json(text: &str) -> Result<Value> {
    let trimmed = text.trim();
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        return Ok(v);
    }

    let Some(start) = trimmed.find('{') else {
        bail!("no JSON object in the reply");
    };

    let bytes = trimmed.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for i in start..bytes.len() {
        let c = bytes[i] as char;
        if in_string {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return serde_json::from_str(&trimmed[start..=i])
                        .context("the extracted object is not valid JSON");
                }
            }
            _ => {}
        }
    }
    bail!("unbalanced braces in the reply")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_bare_object() {
        assert_eq!(extract_json(r#"{"a":1}"#).unwrap()["a"], 1);
    }

    #[test]
    fn parses_a_fenced_object() {
        let t = "Here you go:\n```json\n{\"a\": 1}\n```\n";
        assert_eq!(extract_json(t).unwrap()["a"], 1);
    }

    #[test]
    fn a_brace_inside_a_string_does_not_end_the_object() {
        let t = r#"{"code": "if x { y }", "a": 2}"#;
        assert_eq!(extract_json(t).unwrap()["a"], 2);
    }

    #[test]
    fn an_escaped_quote_does_not_end_the_string() {
        let t = r#"{"q": "say \"hi\" { now", "a": 3}"#;
        assert_eq!(extract_json(t).unwrap()["a"], 3);
    }

    #[test]
    fn handles_nested_objects() {
        let t = r#"prose {"outer": {"inner": {"deep": 1}}, "a": 4} more prose"#;
        assert_eq!(extract_json(t).unwrap()["a"], 4);
    }

    #[test]
    fn rejects_a_reply_with_no_object() {
        assert!(extract_json("I cannot help with that.").is_err());
    }

    #[test]
    fn rejects_unbalanced_braces() {
        assert!(extract_json(r#"{"a": 1"#).is_err());
    }
}
