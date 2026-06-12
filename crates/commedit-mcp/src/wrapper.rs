//! `Yaml<T>` — a tool-result wrapper like rmcp's `Json<T>`, but the result is a
//! single YAML text content block instead of compact JSON, so it reads cleanly
//! in a chat transcript. No `structured_content` / `outputSchema` is emitted:
//! MCP clients that receive structured content surface it as JSON and hide the
//! text block, so YAML is the sole, human-facing wire form.

use std::borrow::Cow;

use rmcp::handler::server::tool::IntoCallToolResult;
use rmcp::model::{CallToolResult, Content};
use rmcp::ErrorData;
use schemars::JsonSchema;
use serde::Serialize;

/// Wrap a serializable response so the tool result carries it as a single YAML
/// text content block. Mirrors `rmcp::handler::server::wrapper::Json`, down to
/// delegating its `JsonSchema` to `T`, but emits no JSON `structured_content`.
pub struct Yaml<T>(pub T);

impl<T: JsonSchema> JsonSchema for Yaml<T> {
    fn schema_name() -> Cow<'static, str> {
        T::schema_name()
    }
    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        T::json_schema(generator)
    }
}

impl<T: Serialize + JsonSchema> IntoCallToolResult for Yaml<T> {
    fn into_call_tool_result(self) -> Result<CallToolResult, ErrorData> {
        let value = serde_yaml::to_value(&self.0).map_err(|e| {
            ErrorData::internal_error(format!("serializing YAML content: {e}"), None)
        })?;
        let yaml = serde_yaml::to_string(&unfold_blobs(value)).map_err(|e| {
            ErrorData::internal_error(format!("serializing YAML content: {e}"), None)
        })?;
        // No `structured_content`: a client that receives it surfaces the JSON
        // and hides this text block, defeating the readable YAML rendering.
        Ok(CallToolResult::success(vec![Content::text(yaml)]))
    }
}

/// serde_yaml renders a multi-line string as a readable literal block scalar
/// (`|`) only when no line carries a tab or trailing whitespace; otherwise it
/// falls back to an escaped one-line `"a\nb\n…"` scalar that is unreadable in a
/// transcript (a diff with tabs, or a whitespace-only edit, hits this). For such
/// values, present the string as a YAML *sequence* of its lines instead — each
/// line on its own row, individually quoted only as needed — which stays
/// readable and recovers the text by joining the entries with newlines.
/// Block-renderable strings are left as-is.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dto::SaveResultDto;

    #[test]
    fn the_result_is_a_single_yaml_block_with_no_structured_content() {
        let dto = SaveResultDto::Clean {
            head_sha: Some("abc123".into()),
        };
        let result = Yaml(dto).into_call_tool_result().expect("into result");

        // No machine-readable JSON half: a client would surface it and hide the
        // YAML, so the result is the YAML text block alone.
        assert!(result.structured_content.is_none());
        assert_eq!(result.content.len(), 1);

        // That block is valid YAML carrying the data, status tag and all.
        let text = &result.content[0].as_text().expect("text content").text;
        let parsed: serde_json::Value =
            serde_yaml::from_str(text).expect("the text block is valid YAML");
        assert_eq!(parsed["status"], "clean");
        assert_eq!(parsed["head_sha"], "abc123");
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
        assert!(
            text.contains("clean: |"),
            "clean should be a block scalar:\n{text}"
        );
        // ...the tab-bearing one is unfolded into a sequence of lines, never an
        // escaped one-line "...\n..." blob.
        assert!(
            text.contains("blobby:\n"),
            "blobby should be a sequence:\n{text}"
        );
        assert!(
            !text.contains("\\n"),
            "no escaped newlines anywhere:\n{text}"
        );

        // Both forms stay lossless YAML — the block scalar verbatim, the
        // sequence as one string per line.
        let parsed: serde_json::Value =
            serde_yaml::from_str(text).expect("the text block is valid YAML");
        assert_eq!(parsed["clean"], "fn main() {\n    ok\n}");
        assert_eq!(
            parsed["blobby"],
            serde_json::json!(["fn main() {", "\tok", "}"])
        );
    }
}
