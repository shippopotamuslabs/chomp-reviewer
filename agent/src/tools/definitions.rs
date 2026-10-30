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
    tool_specs_for(&TOOL_NAMES)
}

/// The synthesizer-only tool name set (FRG-27): the four FRG-18 read
/// tools plus the `run_pr_agent` follow-up. Lens sessions provably
/// never see this set — they build from [`TOOL_NAMES`], and the
/// consistency test asserts the exclusion.
pub const SYNTHESIS_TOOL_NAMES: [&str; 5] = [
    "list_dir",
    "grep",
    "read_file",
    "read_symbol",
    "run_pr_agent",
];

/// The schema channel text for the synthesizer session (FRG-27): the
/// four read-tool entries plus the follow-up. Like
/// [`TOOLS_SCHEMA_JSON`], parsed at the session boundary.
pub const SYNTHESIS_SCHEMA_JSON: &str = r#"[
  {"name":"list_dir","description":"List one directory of the review snapshot: paths are repo-relative and confined to the snapshot, and at most 200 entries are returned sorted by name.","parameters":{"type":"object","properties":{"path":{"type":"string","description":"Repo-relative directory to list; \".\" for the snapshot root."}},"required":["path"],"additionalProperties":false}},
  {"name":"grep","description":"Search snapshot file contents with a regular expression: paths are repo-relative and confined to the snapshot, and at most 50 hits are returned with 200-character snippets.","parameters":{"type":"object","properties":{"pattern":{"type":"string","description":"Regular expression matched against each text line."},"glob":{"type":"string","description":"Optional glob: matched against the file name when it has no slash, else the full repo-relative path."},"path":{"type":"string","description":"Optional file or directory scope; defaults to the snapshot root."}},"required":["pattern"],"additionalProperties":false}},
  {"name":"read_file","description":"Read a numbered window of one snapshot text file: paths are repo-relative and confined to the snapshot, at most 200 lines are returned per call, and binary files are refused.","parameters":{"type":"object","properties":{"path":{"type":"string","description":"Repo-relative file path."},"start_line":{"type":"integer","minimum":1,"description":"Optional 1-based first line; defaults to 1."},"end_line":{"type":"integer","minimum":1,"description":"Optional 1-based last line; defaults to start_line plus 199."}},"required":["path"],"additionalProperties":false}},
  {"name":"read_symbol","description":"Look up a symbol definition in the snapshot and read a 200-line window around it: paths are repo-relative and confined to the snapshot, and an ambiguous name returns up to 10 candidates.","parameters":{"type":"object","properties":{"name":{"type":"string","description":"Symbol name; qualified forms search the last segment."},"path":{"type":"string","description":"Optional file or directory hint; defaults to the snapshot root."}},"required":["name"],"additionalProperties":false}},
  {"name":"run_pr_agent","description":"Ask PR-Agent for a focused second look at a code scope: runs the PR-Agent review with your scope and focus question as extra instructions, and returns its guide text. The host allows at most 2 calls per review; further calls fail softly without running anything.","parameters":{"type":"object","properties":{"scope":{"type":"array","items":{"type":"string"},"description":"Repo-relative file paths to focus the second look on; must not be empty."},"question":{"type":"string","description":"The focused question for PR-Agent to answer about the scope."}},"required":["scope","question"],"additionalProperties":false}}
]"#;

/// One [`ToolSpec`] per [`SYNTHESIS_TOOL_NAMES`] entry. The follow-up
/// is not reproducible (it spends a live model call with a head-side
/// cap); the read tools keep the lens semantics.
pub fn synthesis_tool_specs() -> Vec<ToolSpec> {
    let mut specs = tool_specs_for(&SYNTHESIS_TOOL_NAMES);
    for spec in &mut specs {
        if spec.name.as_str() == "run_pr_agent" {
            spec.reproducible = false;
        }
    }
    specs
}

/// The catalog value the synthesizer session drops into
/// `AgentConfig.tool_semantics`.
pub fn synthesis_tool_catalog_config() -> ToolCatalogConfig {
    ToolCatalogConfig {
        tools: synthesis_tool_specs(),
    }
}

fn tool_specs_for(names: &[&str]) -> Vec<ToolSpec> {
    names
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

    // -- FRG-27: the synthesizer-only follow-up -------------------------------

    #[test]
    fn synthesis_tool_set_adds_only_the_followup() {
        use agent_core::engine::SessionIoParams;

        // The synthesis schema parses at the session boundary.
        SessionIoParams::new(
            "openrouter".to_string(),
            200_000,
            SYNTHESIS_SCHEMA_JSON.to_string(),
            "system".to_string(),
        )
        .expect("synthesis tool schema parses");

        let names = |entries: &[serde_json::Value]| {
            entries
                .iter()
                .map(|entry| {
                    entry["name"]
                        .as_str()
                        .expect("schema entry has a name")
                        .to_string()
                })
                .collect::<Vec<_>>()
        };
        let lens_schema: serde_json::Value =
            serde_json::from_str(TOOLS_SCHEMA_JSON).expect("lens schema is JSON");
        let synthesis_schema: serde_json::Value =
            serde_json::from_str(SYNTHESIS_SCHEMA_JSON).expect("synthesis schema is JSON");

        // The lens set provably excludes the follow-up: schema, specs,
        // catalog, and the name array all lack it.
        for set in [
            names(lens_schema.as_array().expect("lens schema is an array")),
            tool_specs()
                .iter()
                .map(|spec| spec.name.as_str().to_string())
                .collect(),
            tool_catalog_config()
                .tools
                .iter()
                .map(|spec| spec.name.as_str().to_string())
                .collect(),
            TOOL_NAMES.iter().map(|name| name.to_string()).collect(),
        ] {
            assert!(
                !set.contains(&"run_pr_agent".to_string()),
                "lens tool set excludes the follow-up: {set:?}"
            );
        }

        // The synthesis set is exactly the lens set plus the follow-up.
        let mut expected: Vec<String> = TOOL_NAMES.iter().map(|name| name.to_string()).collect();
        expected.push("run_pr_agent".to_string());
        expected.sort();
        for set in [
            names(
                synthesis_schema
                    .as_array()
                    .expect("synthesis schema is an array"),
            ),
            synthesis_tool_specs()
                .iter()
                .map(|spec| spec.name.as_str().to_string())
                .collect(),
            synthesis_tool_catalog_config()
                .tools
                .iter()
                .map(|spec| spec.name.as_str().to_string())
                .collect(),
            SYNTHESIS_TOOL_NAMES
                .iter()
                .map(|name| name.to_string())
                .collect(),
        ] {
            let mut sorted = set.clone();
            sorted.sort();
            assert_eq!(sorted, expected, "synthesis set is lens plus follow-up");
        }

        // The follow-up entry carries scope+question, both required,
        // with a non-empty description; only it is non-reproducible.
        let entry = synthesis_schema
            .as_array()
            .expect("synthesis schema is an array")
            .iter()
            .find(|entry| entry["name"] == "run_pr_agent")
            .expect("synthesis schema has the follow-up");
        let required: Vec<String> = entry["parameters"]["required"]
            .as_array()
            .expect("follow-up has a required array")
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .expect("required entries are strings")
                    .to_string()
            })
            .collect();
        assert_eq!(required, vec!["scope", "question"]);
        assert!(
            entry["description"]
                .as_str()
                .is_some_and(|description| !description.is_empty()),
            "follow-up has a non-empty description"
        );
        for spec in synthesis_tool_specs() {
            assert_eq!(
                spec.reproducible,
                spec.name.as_str() != "run_pr_agent",
                "{} reproducibility",
                spec.name.as_str()
            );
        }
    }
}
