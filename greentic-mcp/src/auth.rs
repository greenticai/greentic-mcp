use crate::protocol::{AuthMode, McpServerConfig, ProtocolRevision};
use std::collections::HashMap;
use std::sync::Mutex;
use tracing::warn;

/// Minimal OAuth broker interface for obtaining scoped tokens.
pub trait OAuthBroker: Send + Sync {
    fn fetch_token(
        &self,
        provider: &str,
        resource: &str,
        scopes: &[String],
    ) -> Result<String, String>;
}

type TokenCacheKey = (String, String, Vec<String>);

/// Simple cache wrapper to avoid repeated broker calls for the same tuple.
pub struct CachedBroker<B: OAuthBroker> {
    broker: B,
    cache: Mutex<HashMap<TokenCacheKey, String>>,
}

impl<B: OAuthBroker> CachedBroker<B> {
    pub fn new(broker: B) -> Self {
        Self {
            broker,
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn get_token(
        &self,
        provider: &str,
        resource: &str,
        scopes: &[String],
    ) -> Result<String, String> {
        let key: TokenCacheKey = (provider.to_string(), resource.to_string(), scopes.to_vec());
        if let Some(tok) = self.cache.lock().unwrap().get(&key) {
            return Ok(tok.clone());
        }
        let token = self.broker.fetch_token(provider, resource, scopes)?;
        self.cache.lock().unwrap().insert(key, token.clone());
        Ok(token)
    }
}

/// Retrieve a token for a server, enforcing resource requirements for 2025-06.
pub fn fetch_oauth_token<B: OAuthBroker>(
    broker: &B,
    server: &McpServerConfig,
    revision: ProtocolRevision,
) -> Result<String, String> {
    let auth_mode = server.resolved_auth_mode();
    if auth_mode != AuthMode::OAuth {
        return Err("auth_mode is not OAuth".into());
    }
    let oauth = server
        .oauth
        .as_ref()
        .ok_or_else(|| "missing oauth config".to_string())?;

    let resource = oauth.resource.as_deref().unwrap_or("").trim().to_string();
    if resource.is_empty() {
        if revision == ProtocolRevision::V2025_06_18 {
            return Err(format!(
                "server '{}' requires oauth.resource for protocol {}",
                server.name,
                revision.as_str()
            ));
        } else {
            warn!(
                server = %server.name,
                "oauth.resource is missing; this will be required for newer protocol revisions"
            );
        }
    }

    let resource = if resource.is_empty() {
        ""
    } else {
        resource.as_str()
    };

    broker.fetch_token(&oauth.provider, resource, &oauth.scopes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::OAuthConfig;

    #[derive(Default)]
    struct MockBroker {
        calls: Mutex<Vec<(String, String, Vec<String>)>>,
        token: String,
    }

    impl OAuthBroker for MockBroker {
        fn fetch_token(
            &self,
            provider: &str,
            resource: &str,
            scopes: &[String],
        ) -> Result<String, String> {
            self.calls.lock().unwrap().push((
                provider.to_string(),
                resource.to_string(),
                scopes.to_vec(),
            ));
            Ok(self.token.clone())
        }
    }

    fn server(resource: Option<&str>, rev: ProtocolRevision) -> McpServerConfig {
        McpServerConfig {
            name: "svc".into(),
            protocol_revision: Some(rev),
            auth_mode: AuthMode::OAuth,
            oauth: Some(OAuthConfig {
                provider: "auth0".into(),
                resource: resource.map(|s| s.to_string()),
                scopes: vec!["a".into(), "b".into()],
                extra: Default::default(),
            }),
            api_key: None,
            bearer_token: None,
            extra: Default::default(),
        }
    }

    #[test]
    fn enforces_resource_for_new_revision() {
        let mock = MockBroker {
            token: "tok".into(),
            ..Default::default()
        };
        let server = server(None, ProtocolRevision::V2025_06_18);
        let err = fetch_oauth_token(&mock, &server, ProtocolRevision::V2025_06_18).unwrap_err();
        assert!(err.contains("requires oauth.resource"));
    }

    #[test]
    fn fetches_token_and_records_calls() {
        let mock = MockBroker {
            token: "tok".into(),
            ..Default::default()
        };
        let server = server(Some("https://svc"), ProtocolRevision::V2025_06_18);
        let token = fetch_oauth_token(&mock, &server, ProtocolRevision::V2025_06_18).unwrap();
        assert_eq!(token, "tok");
        let calls = mock.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "auth0");
        assert_eq!(calls[0].1, "https://svc");
        assert_eq!(calls[0].2, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn allows_missing_resource_for_legacy_with_warning() {
        let mock = MockBroker {
            token: "tok".into(),
            ..Default::default()
        };
        let server = server(None, ProtocolRevision::V2025_03_26);
        let token = fetch_oauth_token(&mock, &server, ProtocolRevision::V2025_03_26).unwrap();
        assert_eq!(token, "tok");
    }

    #[test]
    fn rejects_non_oauth_auth_modes() {
        let mock = MockBroker {
            token: "tok".into(),
            ..Default::default()
        };
        let server = McpServerConfig {
            name: "svc".into(),
            protocol_revision: Some(ProtocolRevision::V2025_06_18),
            auth_mode: AuthMode::BearerToken,
            oauth: Some(OAuthConfig {
                provider: "auth0".into(),
                resource: Some("https://svc".into()),
                scopes: vec!["a".into()],
                extra: Default::default(),
            }),
            api_key: None,
            bearer_token: None,
            extra: Default::default(),
        };
        let err = fetch_oauth_token(&mock, &server, ProtocolRevision::V2025_06_18)
            .expect_err("not OAuth");
        assert_eq!(err, "auth_mode is not OAuth");
    }

    #[test]
    fn requires_token_for_new_revision_resource_when_empty_and_not_found() {
        let mock = MockBroker {
            token: "tok".into(),
            ..Default::default()
        };
        let server = server(None, ProtocolRevision::V2025_06_18);
        let err = fetch_oauth_token(&mock, &server, ProtocolRevision::V2025_06_18)
            .expect_err("should require resource");
        assert!(err.contains("requires oauth.resource"));
    }
}
