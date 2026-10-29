use agent_core::catalog::ToolSpec;
use agent_core::tool_name::ToolName;
use agent_core::tool_semantics::ToolCatalogConfig;

/// The single source of truth for the FRG-18 tool name set, in schema
/// order. [`TOOLS_SCHEMA_JSON`], [`tool_specs`], and
/// [`tool_catalog_config`] are all built from (or checked against)
/// this array — never from a second hand-typed list.
pub const TOOL_NAMES: [&str; 4] = ["list_dir", "grep", "read_file", "read_symbol"];

/// The schema channel text for the four FRG-18 tools, in [`TOOL_NAMES`]
/// order. Each entry carries a `name`, a one-or-two-sentence
/// `description` (what the tool does, that paths are repo-relative and
/// confined to the snapshot, and its hard caps), and a `parameters`
/// object whose `required` array matches the corresponding `Args`
/// struct exactly. FRG-19 passes this string to
/// [`SessionIoParams::new`](agent_core::engine::SessionIoParams::new)
/// as `tools_schema_json`.
pub const TOOLS_SCHEMA_JSON: &str = r#"[
  {"name":"list_dir","description":"List one directory of the review snapshot: paths are repo-relative and confined to the snapshot, and at most 200 entries are returned sorted by name.","parameters":{"type":"object","properties":{"path":{"type":"string","description":"Repo-relative directory to list; \".\" for the snapshot root."}},"required":["path"],"additionalProperties":false}},
  {"name":"grep","description":"Search snapshot file contents with a regular expression: paths are repo-relative and confined to the snapshot, and at most 50 hits are returned with 200-character snippets.","parameters":{"type":"object","properties":{"pattern":{"type":"string","description":"Regular expression matched against each text line."},"glob":{"type":"string","description":"Optional glob: matched against the file name when it has no slash, else the full repo-relative path."},"path":{"type":"string","description":"Optional file or directory scope; defaults to the snapshot root."}},"required":["pattern"],"additionalProperties":false}},
  {"name":"read_file","description":"Read a numbered window of one snapshot text file: paths are repo-relative and confined to the snapshot, at most 200 lines are returned per call, and binary files are refused.","parameters":{"type":"object","properties":{"path":{"type":"string","description":"Repo-relative file path."},"start_line":{"type":"integer","minimum":1,"description":"Optional 1-based first line; defaults to 1."},"end_line":{"type":"integer","minimum":1,"description":"Optional 1-based last line; defaults to start_line plus 199."}},"required":["path"],"additionalProperties":false}},
  {"name":"read_symbol","description":"Look up a symbol definition in the snapshot and read a 200-line window around it: paths are repo-relative and confined to the snapshot, and an ambiguous name returns up to 10 candidates.","parameters":{"type":"object","properties":{"name":{"type":"string","description":"Symbol name; qualified forms search the last segment."},"path":{"type":"string","description":"Optional file or directory hint; defaults to the snapshot root."}},"required":["name"],"additionalProperties":false}}
]"#;

/// One [`ToolSpec`] per [`TOOL_NAMES`] entry, built from [`TOOL_NAMES`]
/// itself. Every tool is reproducible (same arguments over the same
/// snapshot yield the same result), carries no aliases, and needs no
/// schema hint — the model-facing shape lives in [`TOOLS_SCHEMA_JSON`].
pub fn tool_specs() -> Vec<ToolSpec> {
    TOOL_NAMES
        .iter()
        .map(|name| ToolSpec {
            name: ToolName::new(name).expect("static tool names are valid"),
            aliases: Vec::new(),
            reproducible: true,
            schema_hint: None,
        })
        .collect()
}

/// The catalog value FRG-19 drops into `AgentConfig.tool_semantics`:
/// the FRG-18 tool set with no discovery extras.
pub fn tool_catalog_config() -> ToolCatalogConfig {
    ToolCatalogConfig {
        tools: tool_specs(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::super::ToolExecutor;
    use super::*;

    // -- FRG-18 slice 6: tool definitions consistency ------------------------

    #[test]
    fn tool_definitions_are_consistent() {
        use agent_core::engine::SessionIoParams;

        // 1. The schema channel parses at the session boundary.
        SessionIoParams::new(
            "openrouter".to_string(),
            200_000,
            TOOLS_SCHEMA_JSON.to_string(),
            "system".to_string(),
        )
        .expect("tool schema parses");

        // 2. The same name set everywhere, with no duplicates.
        let schema: serde_json::Value =
            serde_json::from_str(TOOLS_SCHEMA_JSON).expect("tool schema is JSON");
        let schema_names: Vec<String> = schema
            .as_array()
            .expect("tool schema is an array")
            .iter()
            .map(|entry| {
                entry["name"]
                    .as_str()
                    .expect("schema entry has a name")
                    .to_string()
            })
            .collect();
        let spec_names: Vec<String> = tool_specs()
            .iter()
            .map(|spec| spec.name.as_str().to_string())
            .collect();
        let catalog_names: Vec<String> = tool_catalog_config()
            .tools
            .iter()
            .map(|spec| spec.name.as_str().to_string())
            .collect();
        let tool_names: Vec<String> = TOOL_NAMES.iter().map(|name| name.to_string()).collect();
        for names in [&schema_names, &spec_names, &catalog_names, &tool_names] {
            let mut sorted = names.clone();
            sorted.sort();
            sorted.dedup();
            assert_eq!(sorted.len(), 4, "no duplicate tool names: {names:?}");
        }
        let mut expected = tool_names.clone();
        expected.sort();
        for names in [&schema_names, &spec_names, &catalog_names] {
            let mut sorted = names.clone();
            sorted.sort();
            assert_eq!(sorted, expected, "name set matches TOOL_NAMES");
        }

        // 3. Dispatch covers exactly the schema names: every TOOL_NAMES
        // entry with empty arguments fails softly on the arguments —
        // never as an unknown tool — while a bogus name still does.
        let fixture = Fixture::new("tooldefs");
        let executor = fixture.executor();
        for name in TOOL_NAMES {
            let result = executor.execute(&call(name, "{}"));
            assert!(
                !result.result_json.contains("unknown tool"),
                "{name} dispatches: {}",
                result.result_json
            );
        }
        let bogus = executor.execute(&call("no_such_tool", "{}"));
        assert!(bogus.soft_failure);
        assert!(
            bogus.result_json.contains("unknown tool"),
            "{}",
            bogus.result_json
        );

        // 4. Every entry's required array matches its Args struct, and
        // every entry carries a non-empty description.
        let expected_required = [
            ("list_dir", vec!["path"]),
            ("grep", vec!["pattern"]),
            ("read_file", vec!["path"]),
            ("read_symbol", vec!["name"]),
        ];
        assert_eq!(
            schema.as_array().expect("tool schema is an array").len(),
            expected_required.len(),
            "schema has exactly one entry per tool"
        );
        for (name, required) in expected_required {
            let entry = schema
                .as_array()
                .expect("tool schema is an array")
                .iter()
                .find(|entry| entry["name"] == name)
                .unwrap_or_else(|| panic!("schema has an entry for {name}"));
            let actual: Vec<String> = entry["parameters"]["required"]
                .as_array()
                .unwrap_or_else(|| panic!("{name} parameters has a required array"))
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .unwrap_or_else(|| panic!("{name} required entries are strings"))
                        .to_string()
                })
                .collect();
            assert_eq!(actual, required, "{name} required matches its Args struct");
            assert!(
                entry["description"]
                    .as_str()
                    .is_some_and(|description| !description.is_empty()),
                "{name} has a non-empty description"
            );
        }
    }
}
