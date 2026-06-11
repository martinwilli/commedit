//! `Yaml<T>` — a tool-result wrapper like rmcp's `Json<T>`, but the human-facing
//! text content block is YAML instead of compact JSON, so results read cleanly
//! in a chat transcript. `structured_content` (and the advertised `outputSchema`)
//! stay JSON, so the machine-readable contract agents rely on is unchanged.

use std::any::Any;
use std::borrow::Cow;
use std::sync::Arc;

use rmcp::handler::server::tool::IntoCallToolResult;
use rmcp::model::{CallToolResult, Content, JsonObject};
use rmcp::ErrorData;
use schemars::JsonSchema;
use serde::Serialize;

/// Wrap a serializable response so the tool result carries it as YAML text plus
/// JSON `structured_content`. Mirrors `rmcp::handler::server::wrapper::Json`,
/// down to delegating its `JsonSchema` to `T`.
pub struct Yaml<T>(pub T);

impl<T: JsonSchema> JsonSchema for Yaml<T> {
    fn schema_name() -> Cow<'static, str> {
        T::schema_name()
    }
    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        T::json_schema(generator)
    }
}

impl<T: Serialize + JsonSchema + 'static> IntoCallToolResult for Yaml<T> {
    fn into_call_tool_result(self) -> Result<CallToolResult, ErrorData> {
        let json = serde_json::to_value(&self.0).map_err(|e| {
            ErrorData::internal_error(format!("serializing structured content: {e}"), None)
        })?;
        let value = serde_yaml::to_value(&self.0).map_err(|e| {
            ErrorData::internal_error(format!("serializing YAML content: {e}"), None)
        })?;
        let yaml = serde_yaml::to_string(&unfold_blobs(value)).map_err(|e| {
            ErrorData::internal_error(format!("serializing YAML content: {e}"), None)
        })?;
        // Reuse rmcp's structured constructor (CallToolResult is non-exhaustive),
        // then swap its compact-JSON text mirror for the readable YAML rendering.
        // `structured_content` keeps the exact JSON — the authoritative data.
        let mut result = CallToolResult::structured(json);
        result.content = vec![Content::text(yaml)];
        Ok(result)
    }
}

/// serde_yaml renders a multi-line string as a readable literal block scalar
/// (`|`) only when no line carries a tab or trailing whitespace; otherwise it
/// falls back to an escaped one-line `"a\nb\n…"` scalar that is unreadable in a
/// transcript (a diff with tabs, or a whitespace-only edit, hits this). For such
/// values, present the string as a YAML *sequence* of its lines instead — each
/// line on its own row, individually quoted only as needed — which stays
/// readable and lossless. Block-renderable strings are left as-is, and the
/// exact text is always available in `structured_content` regardless.
fn unfold_blobs(value: serde_yaml::Value) -> serde_yaml::Value {
    use serde_yaml::Value;
    match value {
        Value::String(s) if s.contains('\n') && !renders_as_block_scalar(&s) => {
            Value::Sequence(s.lines().map(|l| Value::String(l.to_string())).collect())
        }
        Value::Sequence(seq) => Value::Sequence(seq.into_iter().map(unfold_blobs).collect()),
        Value::Mapping(m) => {
            Value::Mapping(m.into_iter().map(|(k, v)| (k, unfold_blobs(v))).collect())
        }
        other => other,
    }
}

/// Whether serde_yaml will emit `s` as a literal block scalar: no tab anywhere
/// and no line with trailing whitespace (leading whitespace and empty lines are
/// fine — the emitter handles them with an indentation indicator).
fn renders_as_block_scalar(s: &str) -> bool {
    !s.contains('\t') && s.split('\n').all(|line| line.trim_end() == line)
}

/// The output schema rmcp's `#[tool]` macro derives automatically for a `Json<T>`
/// return. `Yaml<T>` isn't the `Json` identifier the macro keys on (rmcp-macros'
/// `extract_json_inner_type`), so each tool re-supplies it via
/// `#[tool(output_schema = ...)]`. Panics at tool-registration time — exactly as
/// the macro's own generated code does — if `T`'s schema is invalid.
pub fn output_schema<T: JsonSchema + Any>() -> Arc<JsonObject> {
    rmcp::handler::server::tool::schema_for_output::<T>().unwrap_or_else(|e| {
        panic!("invalid output schema for {}: {e}", std::any::type_name::<T>())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dto::SaveResultDto;

    #[test]
    fn yaml_text_mirrors_the_json_structured_content() {
        let dto = SaveResultDto::Clean { head_sha: Some("abc123".into()) };
        let result = Yaml(dto).into_call_tool_result().expect("into result");

        // The machine-readable half stays JSON, status tag and all.
        let structured = result.structured_content.expect("structured content present");
        assert_eq!(structured["status"], "clean");
        assert_eq!(structured["head_sha"], "abc123");

        // The displayed half is a single YAML block carrying the same data.
        assert_eq!(result.content.len(), 1);
        let text = &result.content[0].as_text().expect("text content").text;
        let from_yaml: serde_json::Value =
            serde_yaml::from_str(text).expect("the text block is valid YAML");
        assert_eq!(from_yaml, structured);
    }

    #[test]
    fn unreadable_multiline_strings_become_yaml_sequences() {
        #[derive(serde::Serialize, schemars::JsonSchema)]
        struct Probe {
            clean: String,
            blobby: String,
        }
        let result = Yaml(Probe {
            clean: "fn main() {\n    ok\n}".to_string(),
            blobby: "fn main() {\n\tok\n}".to_string(),
        })
        .into_call_tool_result()
        .expect("into result");

        let text = &result.content[0].as_text().expect("text content").text;
        // The tab-free field stays a readable literal block scalar...
        assert!(text.contains("clean: |"), "clean should be a block scalar:\n{text}");
        // ...the tab-bearing one is unfolded into a sequence of lines, never an
        // escaped one-line "...\n..." blob.
        assert!(text.contains("blobby:\n"), "blobby should be a sequence:\n{text}");
        assert!(!text.contains("\\n"), "no escaped newlines anywhere:\n{text}");

        // structured_content keeps both as exact JSON strings regardless.
        let sc = result.structured_content.expect("structured content present");
        assert_eq!(sc["clean"], "fn main() {\n    ok\n}");
        assert_eq!(sc["blobby"], "fn main() {\n\tok\n}");
    }
}
