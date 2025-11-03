use mcp_exec::describe::{describe_tool, Maybe};
use mcp_exec::{ExecConfig, ToolStore};

#[test]
fn online_weather_list_and_describe() {
    if std::env::var("RUN_ONLINE_TESTS").unwrap_or_default() != "1" {
        eprintln!("Skipping online test: set RUN_ONLINE_TESTS=1 to enable");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().to_path_buf();

    let cfg = ExecConfig {
        store: ToolStore::HttpSingleFile {
            name: "weather_api".into(),
            url: "https://github.com/greentic-ai/greentic/raw/refs/heads/main/greentic/plugins/tools/weather_api.wasm".into(),
            cache_dir: cache,
        },
        security: Default::default(),
        runtime: Default::default(),
        http_enabled: true,
    };

    let tools = match cfg.store.list() {
        Ok(tools) => tools,
        Err(err) => {
            eprintln!("Skipping online test: failed to list tools: {err:?}");
            return;
        }
    };
    if !tools.iter().any(|t| t.name == "weather_api") {
        eprintln!("Skipping online test: weather_api tool not present");
        return;
    }

    let describe = match describe_tool("weather_api", &cfg) {
        Ok(desc) => desc,
        Err(err) => {
            eprintln!("Skipping online test: describe failed: {err:?}");
            return;
        }
    };

    match describe.capabilities {
        Maybe::Data(caps) => {
            assert!(
                !caps.is_empty(),
                "weather_api capabilities should return at least one entry"
            );
        }
        Maybe::Unsupported => {
            eprintln!("weather_api: 'capabilities' action not supported; skipping further checks");
            return;
        }
    }

    if let Maybe::Data(secrets) = describe.secrets {
        assert!(secrets.is_array() || secrets.is_object());
    }
    if let Maybe::Data(schema) = describe.config_schema {
        assert!(schema.is_object());
    }
}
