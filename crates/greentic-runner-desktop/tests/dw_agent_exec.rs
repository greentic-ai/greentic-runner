// Proves that `dw.agent` flow nodes are dispatched to the agentic-worker
// runtime (ephemeral, in-memory) by the desktop runner when `GREENTIC_AW_REDIS_URL`
// is unset. The LLM call is expected to error (no API key in CI), but the
// failure must be recorded AGAINST the `ask` node — proving the handler is wired
// and the node dispatches — rather than failing as "unknown node kind".

#[cfg(feature = "desktop-agent-ephemeral")]
mod tests {
    use std::collections::BTreeMap;
    use std::fs::File;
    use std::io::Write;
    use std::path::Path;

    use anyhow::{Context, Result};
    use greentic_runner_desktop::{RunOptions, RunStatus, desktop_defaults, run_pack_with_options};
    use greentic_types::{
        ComponentCapabilities, ComponentManifest, ComponentProfiles, ExtensionInline, ExtensionRef,
        PackFlowEntry, PackKind, PackManifest, ResourceHints, encode_pack_manifest,
    };
    use once_cell::sync::Lazy;
    use semver::Version;
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use zip::write::FileOptions;

    const RUNTIME_FLOW_EXT_ID: &str = "greentic.pack.runtime_flow";
    const PACK_ID: &str = "dw.agent.desktop.test";
    const FLOW_ID: &str = "greet.flow";
    const AGENT_ID: &str = "greeter";

    static COMPONENT_ARTIFACT: Lazy<Vec<u8>> = Lazy::new(|| {
        let workspace = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.to_path_buf())
            .expect("workspace root");
        // Use the fixture pack that the runner-host tests also use.
        let archive_path =
            workspace.join("tests/fixtures/packs/runner-components/runner-components.gtpack");
        let file = File::open(&archive_path).expect("open fixture gtpack");
        let mut archive = zip::ZipArchive::new(file).expect("parse fixture gtpack");
        let mut entry = archive
            .by_name("components/qa.process@0.1.0/component.wasm")
            .expect("qa.process wasm missing from fixture pack");
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut buf).expect("read wasm bytes");
        buf
    });

    fn agent_blob() -> Value {
        json!({
            "agent_id": AGENT_ID,
            "system_prompt": "You are a terse greeter. Reply with exactly: hi",
            "tools": [],
            "guardrails": [],
            "llm": {
                "provider": "openai",
                "model": "gpt-4o-mini"
            },
            "limits": {}
        })
    }

    fn build_dw_agent_pack(pack_path: &Path) -> Result<()> {
        let node = json!({
            "component": "dw.agent",
            "operation": AGENT_ID,
            "input": { "user_text": "{{in.text}}" },
            "routing": "end",
        });
        let mut nodes = serde_json::Map::new();
        nodes.insert("ask".to_string(), node);

        let runtime_flow = json!({
            "id": FLOW_ID,
            "flow_type": "messaging",
            "start": "ask",
            "nodes": Value::Object(nodes),
        });
        let runtime_ext_value = json!({ "flows": [runtime_flow] });

        let mut extensions = BTreeMap::new();
        extensions.insert(
            RUNTIME_FLOW_EXT_ID.to_string(),
            ExtensionRef {
                kind: RUNTIME_FLOW_EXT_ID.to_string(),
                version: "2.0.0".into(),
                digest: None,
                location: None,
                inline: Some(ExtensionInline::Other(runtime_ext_value)),
            },
        );

        let mut agents: BTreeMap<String, Value> = BTreeMap::new();
        agents.insert(AGENT_ID.to_string(), agent_blob());

        let manifest = PackManifest {
            schema_version: "1.0".into(),
            pack_id: PACK_ID.parse().expect("valid pack id"),
            name: None,
            version: Version::parse("0.0.0").expect("valid version"),
            kind: PackKind::Application,
            publisher: "test".into(),
            components: vec![ComponentManifest {
                id: "qa.process".parse().expect("component id"),
                version: Version::parse("0.1.0").expect("valid version"),
                supports: vec![greentic_types::FlowKind::Messaging],
                world: "greentic:component@0.4.0".into(),
                profiles: ComponentProfiles::default(),
                capabilities: ComponentCapabilities::default(),
                configurators: None,
                operations: Vec::new(),
                config_schema: None,
                resources: ResourceHints::default(),
                dev_flows: BTreeMap::new(),
            }],
            flows: Vec::<PackFlowEntry>::new(),
            dependencies: Vec::new(),
            capabilities: Vec::new(),
            signatures: Default::default(),
            secret_requirements: Vec::new(),
            bootstrap: None,
            agents,
            extensions: Some(extensions),
        };

        let mut zip = zip::ZipWriter::new(File::create(pack_path).context("create pack archive")?);
        let options: FileOptions<'_, ()> =
            FileOptions::default().compression_method(zip::CompressionMethod::Stored);

        let manifest_bytes = encode_pack_manifest(&manifest)?;
        zip.start_file("manifest.cbor", options)?;
        zip.write_all(&manifest_bytes)?;

        // The pack loader requires a real component artifact to validate; the
        // dw.agent node never invokes it — it routes through the agent handler.
        zip.start_file("components/qa.process.wasm", options)?;
        zip.write_all(&COMPONENT_ARTIFACT)?;

        zip.finish().context("finalise pack archive")?;
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    #[allow(unsafe_code)]
    fn desktop_dispatches_dw_agent_node() {
        // SAFETY: #[serial] ensures no concurrent env mutation in this test suite.
        unsafe {
            std::env::remove_var("GREENTIC_AW_REDIS_URL");
        }

        let temp = TempDir::new().expect("tempdir");
        let pack_path = temp.path().join("dw-agent-test.gtpack");
        build_dw_agent_pack(&pack_path).expect("build fixture pack");

        let opts = RunOptions {
            entry_flow: Some(FLOW_ID.to_string()),
            input: json!({ "text": "hi" }),
            ..desktop_defaults()
        };

        let result = run_pack_with_options(&pack_path, opts)
            .expect("run_pack_with_options should return Ok");

        // The `ask` node must appear in node_summaries, proving the engine
        // dispatched it to the dw.agent handler rather than rejecting it as an
        // unknown component. With no OPENAI_API_KEY the node will fail, but the
        // failure is recorded against the node — NOT as a top-level "unknown
        // node kind" error.
        let node_ids: Vec<&str> = result
            .node_summaries
            .iter()
            .map(|n| n.node_id.as_str())
            .collect();
        assert!(
            node_ids.contains(&"ask"),
            "dw.agent node 'ask' was not dispatched; node_summaries={node_ids:?}, \
             status={:?}, error={:?}",
            result.status,
            result.error
        );

        // Any outcome except "unknown node kind" or "unresolved component" proves wiring.
        if let Some(failure) = result.failures.get("ask") {
            let msg = failure.message.to_ascii_lowercase();
            assert!(
                !msg.contains("unknown node kind")
                    && !msg.contains("unresolved component")
                    && !msg.contains("no handler"),
                "dw.agent node failed with routing/wiring error rather than LLM error: {msg}"
            );
        }

        // The run either succeeds (unlikely without API key) or fails at the LLM
        // call — either is acceptable. What must NOT happen is a Failure with no
        // node_summaries (that would mean the pack couldn't even be loaded with
        // the agent wired in).
        if result.status == RunStatus::Failure && result.node_summaries.is_empty() {
            panic!(
                "run failed before reaching any node — pack load or wiring issue. \
                 error={:?}",
                result.error
            );
        }
    }
}
