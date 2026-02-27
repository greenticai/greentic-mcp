use greentic_mcp_exec::describe::{Maybe, describe_tool};
use greentic_mcp_exec::{ExecConfig, ToolStore};

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
            url: "https://github.com/greenticai/greentic/raw/refs/heads/main/greentic/plugins/tools/weather_api.wasm".into(),
            cache_dir: cache,
        },
        security: Default::default(),
        runtime: Default::default(),
        http_enabled: true,
        secrets_store: None,
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

    let greentic_mcp_exec::describe::ToolDescribe {
        describe_v1,
        capabilities,
        secrets,
        config_schema,
        secret_requirements,
    } = describe;

    if let Some(doc) = describe_v1 {
        assert!(doc.get("name").is_some(), "describe-json should set a name");
        assert!(
            doc.get("versions").is_some(),
            "describe-json should include versions"
        );
        return;
    }

    match capabilities {
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

    if let Maybe::Data(secrets) = secrets {
        assert!(secrets.is_array() || secrets.is_object());
    }
    if let Maybe::Data(schema) = config_schema {
        assert!(schema.is_object());
    }
    assert!(
        !secret_requirements
            .iter()
            .any(|req| req.key.as_str().is_empty()),
        "secret requirements should not be empty"
    );
}

