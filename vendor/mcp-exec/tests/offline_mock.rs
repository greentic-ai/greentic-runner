use mcp_exec::describe::{describe_tool, Maybe};
use mcp_exec::{ExecConfig, ToolStore, VerifyPolicy};
use std::path::PathBuf;

#[test]
fn offline_mock_describe_and_list() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();

    std::fs::create_dir_all(&dir).unwrap();
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock_tool.wasm");
    std::fs::copy(fixture, dir.join("mock_tool.wasm")).unwrap();

    let cfg = ExecConfig {
        store: ToolStore::LocalDir(dir.clone()),
        security: VerifyPolicy {
            allow_unverified: true,
            ..Default::default()
        },
        runtime: Default::default(),
        http_enabled: false,
    };

    let tools = cfg.store.list().unwrap();
    assert!(tools.iter().any(|t| t.name == "mock_tool"));

    let describe = describe_tool("mock_tool", &cfg).unwrap();

    match describe.capabilities {
        Maybe::Data(caps) => {
            assert!(caps.contains(&"describe".to_string()));
        }
        Maybe::Unsupported => panic!("mock tool should support capabilities"),
    }

    assert!(matches!(describe.secrets, Maybe::Data(_)));
    assert!(matches!(describe.config_schema, Maybe::Data(_)));
}
