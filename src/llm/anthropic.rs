//! Anthropic Messages API over raw HTTP. Rust has no official SDK, so this is
//! the documented wire shape rather than a guessed binding.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";

/// Generation gets the stronger model; judging gets a different one.
/// §8: the generator writing the question, the reference *and* the grade means a
/// wrong premise sails through unchallenged. Different models do not remove
/// correlated error, but they decorrelate it, and the cheaper judge costs less.
pub const GENERATE_MODEL: &str = "claude-opus-5";
pub const JUDGE_MODEL: &str = "claude-sonnet-5";

pub struct Client {
    http: reqwest::blocking::Client,
    api_key: String,
}

#[derive(Deserialize)]
struct Response {
    content: Vec<Block>,
    stop_reason: Option<String>,
    usage: Usage,
}

#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

#[derive(Deserialize, Default, Debug)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

/// Built separately from the request so the wire shape can be asserted without
/// a key or a network call.
pub fn request_body(
    model: &str,
    system: &str,
    user: &str,
    schema: Value,
    effort: &str,
) -> Value {
    json!({
        "model": model,
        "max_tokens": 16000,
        // The diff dominates the token count and is identical across every
        // judge call for a gate, so it is cached once and read thereafter.
        "system": [{
            "type": "text",
            "text": system,
            "cache_control": {"type": "ephemeral"}
        }],
        "messages": [{"role": "user", "content": user}],
        // Adaptive is the only on-mode on these models; budget_tokens and the
        // sampling parameters are rejected with a 400.
        "thinking": {"type": "adaptive"},
        "output_config": {
            "effort": effort,
            // Structured outputs: the response is guaranteed to match the
            // schema, so there are no code fences to strip and no parse retry.
            "format": {"type": "json_schema", "schema": schema}
        }
    })
}

impl Client {
    /// The key is read from the environment. `ant auth login` profiles are not
    /// supported here — there is no Rust SDK to resolve them.
    pub fn from_env() -> Result<Option<Self>> {
        let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") else {
            return Ok(None);
        };
        if api_key.trim().is_empty() {
            return Ok(None);
        }
        Ok(Some(Self {
            // An LLM call with no timeout is an indefinite hang, which in v1
            // stranded a gate forever. reqwest's default is no timeout.
            http: reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(180))
                .build()?,
            api_key,
        }))
    }

    /// One request. `system` is sent as a cached block so that the diff, which
    /// dominates the token count, is written once by `generate` and read by
    /// every judge call.
    pub fn complete(
        &self,
        model: &str,
        system: &str,
        user: &str,
        schema: Value,
        effort: &str,
    ) -> Result<(Value, Usage)> {
        let body = request_body(model, system, user, schema, effort);

        let resp = self
            .http
            .post(API_URL)
            .header("content-type", "application/json")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .json(&body)
            .send()
            .context("request to the Anthropic API failed")?;

        let status = resp.status();
        let text = resp.text()?;
        if !status.is_success() {
            bail!("Anthropic API returned {status}: {}", text.trim());
        }

        let parsed: Response =
            serde_json::from_str(&text).context("could not parse the API response")?;

        // Always check stop_reason before reading content.
        if parsed.stop_reason.as_deref() == Some("refusal") {
            bail!("the model declined this request");
        }

        let body_text = parsed
            .content
            .iter()
            .find(|b| b.kind == "text")
            .map(|b| b.text.as_str())
            .ok_or_else(|| anyhow!("no text block in the response"))?;

        Ok((serde_json::from_str(body_text)?, parsed.usage))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body() -> Value {
        request_body(
            GENERATE_MODEL,
            "system text",
            "user text",
            json!({"type": "object", "properties": {}, "required": [], "additionalProperties": false}),
            "high",
        )
    }

    /// Pins the documented wire shape. Rust has no official SDK, so nothing else
    /// catches a drift between this and the Messages API.
    #[test]
    fn matches_the_documented_request_shape() {
        let b = body();
        assert_eq!(b["model"], GENERATE_MODEL);
        assert_eq!(b["messages"][0]["role"], "user");
        assert_eq!(b["thinking"]["type"], "adaptive");
        // format nests under output_config; the top-level output_format
        // parameter it replaced is deprecated.
        assert_eq!(b["output_config"]["format"]["type"], "json_schema");
        assert!(b["output_config"]["format"]["schema"].is_object());
        assert_eq!(b["output_config"]["effort"], "high");
        assert!(b.get("output_format").is_none());
    }

    /// budget_tokens and the sampling parameters return a 400 on these models.
    #[test]
    fn sends_no_parameter_these_models_reject() {
        let b = body();
        assert!(b["thinking"].get("budget_tokens").is_none());
        for rejected in ["temperature", "top_p", "top_k"] {
            assert!(b.get(rejected).is_none(), "{rejected} must not be sent");
        }
    }

    #[test]
    fn caches_the_system_block() {
        assert_eq!(body()["system"][0]["cache_control"]["type"], "ephemeral");
    }

    /// Structured outputs rejects a schema without additionalProperties: false.
    #[test]
    fn every_schema_object_forbids_extra_properties() {
        fn check(v: &Value, path: &str) {
            if v.get("type").and_then(|t| t.as_str()) == Some("object") {
                assert_eq!(
                    v.get("additionalProperties"),
                    Some(&Value::Bool(false)),
                    "{path} must set additionalProperties: false"
                );
                assert!(v.get("required").is_some(), "{path} must list required");
            }
            if let Some(props) = v.get("properties").and_then(|p| p.as_object()) {
                for (k, sub) in props {
                    check(sub, &format!("{path}.{k}"));
                }
            }
            if let Some(items) = v.get("items") {
                check(items, &format!("{path}[]"));
            }
        }
        check(&crate::llm::generate::schema_for_test(), "generate");
        for (name, s) in crate::llm::judge::schemas_for_test() {
            check(&s, name);
        }
    }

    /// Unsupported constraints return a 400 rather than being ignored.
    #[test]
    fn schemas_avoid_unsupported_constraints() {
        fn check(v: &Value) {
            if let Some(obj) = v.as_object() {
                for bad in ["minimum", "maximum", "minLength", "maxLength", "minItems", "maxItems"] {
                    assert!(obj.get(bad).is_none(), "{bad} is rejected by structured outputs");
                }
                for sub in obj.values() {
                    check(sub);
                }
            }
        }
        check(&crate::llm::generate::schema_for_test());
        for (_, s) in crate::llm::judge::schemas_for_test() {
            check(&s);
        }
    }
}
