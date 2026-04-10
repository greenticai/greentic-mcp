use indexmap::IndexMap;

use crate::types::{McpError, ToolMapConfig, ToolRef};

/// Name to [`ToolRef`] lookup.
#[derive(Clone, Debug)]
pub struct ToolMap {
    tools: IndexMap<String, ToolRef>,
}

impl ToolMap {
    /// Build a [`ToolMap`] from a configuration file.
    pub fn from_config(config: &ToolMapConfig) -> Result<Self, McpError> {
        let mut tools = IndexMap::with_capacity(config.tools.len());
        for tool in &config.tools {
            if tools.contains_key(&tool.name) {
                return Err(McpError::InvalidInput(format!(
                    "duplicate tool name `{}`",
                    tool.name
                )));
            }
            tools.insert(tool.name.clone(), tool.clone());
        }

        Ok(ToolMap { tools })
    }

    /// Retrieve a tool by name.
    pub fn get(&self, name: &str) -> Result<&ToolRef, McpError> {
        self.tools
            .get(name)
            .ok_or_else(|| McpError::tool_not_found(name.to_string()))
    }

    /// Iterate over desired tool references.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &ToolRef)> {
        self.tools.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicates_are_rejected() {
        let config = ToolMapConfig {
            tools: vec![
                ToolRef {
                    name: "echo".into(),
                    component: "a".into(),
                    entry: "tool-invoke".into(),
                    timeout_ms: None,
                    max_retries: None,
                    retry_backoff_ms: None,
                },
                ToolRef {
                    name: "echo".into(),
                    component: "b".into(),
                    entry: "tool-invoke".into(),
                    timeout_ms: None,
                    max_retries: None,
                    retry_backoff_ms: None,
                },
            ],
        };
        let err = ToolMap::from_config(&config).expect_err("duplicate should fail");
        assert!(err.to_string().contains("duplicate tool name"));
    }

    #[test]
    fn get_returns_expected_tool_or_error() {
        let config = ToolMapConfig {
            tools: vec![ToolRef {
                name: "echo".into(),
                component: "a".into(),
                entry: "tool-invoke".into(),
                timeout_ms: Some(100),
                max_retries: Some(1),
                retry_backoff_ms: None,
            }],
        };
        let map = ToolMap::from_config(&config).expect("map");
        let echo = map.get("echo").expect("found");
        assert_eq!(echo.name, "echo");

        let missing = map.get("missing").expect_err("not found");
        assert!(missing.to_string().contains("tool `missing` not found"));
    }

    #[test]
    fn iter_preserves_map_order() {
        let config = ToolMapConfig {
            tools: vec![
                ToolRef {
                    name: "a".into(),
                    component: "a".into(),
                    entry: "tool-invoke".into(),
                    timeout_ms: None,
                    max_retries: None,
                    retry_backoff_ms: None,
                },
                ToolRef {
                    name: "b".into(),
                    component: "b".into(),
                    entry: "tool-invoke".into(),
                    timeout_ms: None,
                    max_retries: None,
                    retry_backoff_ms: None,
                },
            ],
        };
        let map = ToolMap::from_config(&config).expect("map");
        let names: Vec<_> = map.iter().map(|(_, tool)| tool.name.clone()).collect();
        assert_eq!(names, vec!["a".to_string(), "b".to_string()]);
    }
}
