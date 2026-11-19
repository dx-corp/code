//! Runtime authentication for provider-owned managed MCP connections.
//!
//! Managed hosted Orb configuration contains only an opaque connection and
//! credential reference.  This module resolves the EvalOps access token at
//! request time, keeps the fallback snapshot in memory only, and never writes
//! authentication material into an MCP config or connection record.

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use zeroize::Zeroizing;

use super::client::McpError;
use super::config::{McpConfigScope, McpServerConfig};
use crate::credential_mode::{ACCESS_TOKEN_ENV, ACCESS_TOKEN_FILE_ENV};
use crate::init_cli::load_evalops_snapshot;
use crate::orb_connection::validate_hosted_orb_runtime_binding;

#[derive(Default)]
enum SnapshotCache {
    #[default]
    Unloaded,
    Loaded(Option<Zeroizing<String>>),
}

type SnapshotLoader = dyn Fn() -> Result<Option<String>, String> + Send + Sync;

#[cfg(test)]
type RuntimeBindingValidator = dyn Fn(&McpServerConfig) -> Result<(), String> + Send + Sync;

/// Runtime-only token source. The stored EvalOps snapshot is read at most once
/// per source instance until an authentication failure invalidates it.
#[derive(Clone)]
pub(crate) struct EvalOpsAccessTokenSource {
    snapshot: Arc<Mutex<SnapshotCache>>,
    loader: Arc<SnapshotLoader>,
    #[cfg(test)]
    snapshot_only: bool,
}

impl Default for EvalOpsAccessTokenSource {
    fn default() -> Self {
        Self::new()
    }
}

impl EvalOpsAccessTokenSource {
    pub(crate) fn new() -> Self {
        Self::with_loader(|| {
            load_evalops_snapshot()
                .map_err(|error| error.to_string())?
                .map(|snapshot| {
                    stored_snapshot_access(
                        &snapshot.access,
                        snapshot.expires,
                        chrono::Utc::now().timestamp_millis(),
                    )
                })
                .transpose()
                .map(Option::flatten)
                .map_err(|error| error.to_string())
        })
    }

    pub(crate) fn with_loader<F>(loader: F) -> Self
    where
        F: Fn() -> Result<Option<String>, String> + Send + Sync + 'static,
    {
        Self {
            snapshot: Arc::new(Mutex::new(SnapshotCache::Unloaded)),
            loader: Arc::new(loader),
            #[cfg(test)]
            snapshot_only: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_snapshot(access: Option<&str>) -> Self {
        let access = access
            .map(str::trim)
            .filter(|access| !access.is_empty())
            .map(|access| access.to_owned());
        Self::with_loader(move || Ok(access.clone()))
    }

    /// Resolve direct environment, file environment, then cached snapshot.
    ///
    /// Direct and file sources are intentionally read on every request so an
    /// operator can rotate a runtime token without restarting Maestro. The
    /// persisted snapshot is only a read-only fallback and is cached in
    /// process memory; no login or refresh flow is invoked here.
    pub(crate) fn resolve(&self) -> Result<Zeroizing<String>, McpError> {
        #[cfg(test)]
        if self.snapshot_only {
            return self.resolve_snapshot();
        }
        if let Some(access) = environment_value(ACCESS_TOKEN_ENV) {
            return Ok(Zeroizing::new(access));
        }
        if let Some(access) = file_value(ACCESS_TOKEN_FILE_ENV) {
            return Ok(Zeroizing::new(access));
        }

        self.resolve_snapshot()
    }

    fn resolve_snapshot(&self) -> Result<Zeroizing<String>, McpError> {
        let mut cache = self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let SnapshotCache::Loaded(access) = &*cache {
            return access.clone().ok_or_else(missing_access_token);
        }

        let access = (self.loader)().map_err(|error| {
            McpError::ConnectionFailed(format!("EvalOps token unavailable: {error}"))
        })?;
        let access = access
            .map(|access| access.trim().to_owned())
            .filter(|access| !access.is_empty())
            .map(Zeroizing::new);
        *cache = SnapshotCache::Loaded(access.clone());
        access.ok_or_else(missing_access_token)
    }

    pub(crate) fn invalidate(&self) {
        let mut cache = self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *cache = SnapshotCache::Unloaded;
    }
}

fn stored_snapshot_access(
    access: &str,
    expires_at_ms: i64,
    now_ms: i64,
) -> Result<Option<String>, &'static str> {
    let access = access.trim();
    if access.is_empty() {
        return Ok(None);
    }
    if expires_at_ms <= now_ms {
        return Err("stored EvalOps session expired; run `deixic-code evalops login`");
    }
    Ok(Some(access.to_owned()))
}

/// Provider-owned authentication context for managed hosted Orb requests.
#[derive(Clone, Default)]
pub(crate) struct ManagedMcpAuth {
    access_token: EvalOpsAccessTokenSource,
    #[cfg(test)]
    binding_validator: Option<Arc<RuntimeBindingValidator>>,
}

impl ManagedMcpAuth {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Supply a localhost fixture with its own binding validator and token.
    /// Other tests may set process-wide token overrides in parallel.
    #[cfg(test)]
    pub(crate) fn with_loader_for_test<F>(loader: F) -> Self
    where
        F: Fn() -> Result<Option<String>, String> + Send + Sync + 'static,
    {
        Self {
            access_token: EvalOpsAccessTokenSource {
                snapshot_only: true,
                ..EvalOpsAccessTokenSource::with_loader(loader)
            },
            binding_validator: Some(Arc::new(|_| Ok(()))),
        }
    }

    /// Resolve the bearer token for one request after revalidating the exact
    /// managed connection binding. Non-managed/file-backed servers do not use
    /// this provider-owned auth path.
    pub(crate) fn bearer_for(
        &self,
        config: &McpServerConfig,
    ) -> Result<Option<Zeroizing<String>>, McpError> {
        if !requires_hosted_orb_auth(config) {
            return Ok(None);
        }
        #[cfg(test)]
        let binding_result = self
            .binding_validator
            .as_ref()
            .map(|validator| validator(config))
            .unwrap_or_else(|| {
                validate_hosted_orb_runtime_binding(config).map_err(|error| error.to_string())
            });
        #[cfg(not(test))]
        let binding_result =
            validate_hosted_orb_runtime_binding(config).map_err(|error| error.to_string());
        if let Err(error) = binding_result {
            self.invalidate();
            return Err(McpError::ConnectionFailed(format!(
                "hosted Computer managed connection is no longer valid: {error}"
            )));
        }
        self.access_token.resolve().map(Some)
    }

    pub(crate) fn invalidate(&self) {
        self.access_token.invalidate();
        crate::init_cli::invalidate_evalops_credentials_cache();
    }
}

pub(crate) fn requires_hosted_orb_auth(config: &McpServerConfig) -> bool {
    config.scope == McpConfigScope::Managed
        && config.name == crate::orb_connection::HOSTED_ORB_MCP_SERVER_NAME
}

fn environment_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn file_value(name: &str) -> Option<String> {
    let path = environment_value(name)?;
    fs::read_to_string(Path::new(&path))
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn missing_access_token() -> McpError {
    McpError::ConnectionFailed(format!(
        "hosted Computer authentication requires {ACCESS_TOKEN_ENV}, {ACCESS_TOKEN_FILE_ENV}, or a cached EvalOps session"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvRestore {
        values: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvRestore {
        fn capture(names: &[&'static str]) -> Self {
            Self {
                values: names
                    .iter()
                    .map(|name| (*name, std::env::var_os(name)))
                    .collect(),
            }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (name, value) in &self.values {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    struct EvalOpsCredentialCacheReset;

    impl Drop for EvalOpsCredentialCacheReset {
        fn drop(&mut self) {
            crate::init_cli::invalidate_evalops_credentials_cache();
        }
    }

    #[test]
    fn direct_env_precedes_file_and_cached_snapshot() {
        let _guard = crate::config::test_process_env_lock();
        let _restore = EnvRestore::capture(&[ACCESS_TOKEN_ENV, ACCESS_TOKEN_FILE_ENV]);
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), "file-token\n").unwrap();
        std::env::set_var(ACCESS_TOKEN_FILE_ENV, file.path());
        std::env::set_var(ACCESS_TOKEN_ENV, "env-token");

        let source = EvalOpsAccessTokenSource::from_snapshot(Some("snapshot-token"));
        assert_eq!(source.resolve().unwrap().as_str(), "env-token");

        let fixture = ManagedMcpAuth::with_loader_for_test(|| Ok(Some("fixture-token".to_owned())));
        assert_eq!(
            fixture.access_token.resolve().unwrap().as_str(),
            "fixture-token"
        );
    }

    #[test]
    fn file_precedes_cached_snapshot_and_snapshot_is_cached() {
        let _guard = crate::config::test_process_env_lock();
        let _restore = EnvRestore::capture(&[ACCESS_TOKEN_ENV, ACCESS_TOKEN_FILE_ENV]);
        std::env::remove_var(ACCESS_TOKEN_ENV);
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), "file-token\n").unwrap();
        std::env::set_var(ACCESS_TOKEN_FILE_ENV, file.path());

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_for_loader = Arc::clone(&calls);
        let source = EvalOpsAccessTokenSource::with_loader(move || {
            calls_for_loader.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Some("snapshot-token".to_owned()))
        });
        assert_eq!(source.resolve().unwrap().as_str(), "file-token");

        std::fs::write(file.path(), "file-token-2\n").unwrap();
        assert_eq!(source.resolve().unwrap().as_str(), "file-token-2");
        std::env::remove_var(ACCESS_TOKEN_FILE_ENV);
        assert_eq!(source.resolve().unwrap().as_str(), "snapshot-token");
        assert_eq!(source.resolve().unwrap().as_str(), "snapshot-token");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn invalidation_clears_only_the_cached_snapshot() {
        let _guard = crate::config::test_process_env_lock();
        let _restore = EnvRestore::capture(&[ACCESS_TOKEN_ENV, ACCESS_TOKEN_FILE_ENV]);
        std::env::remove_var(ACCESS_TOKEN_ENV);
        std::env::remove_var(ACCESS_TOKEN_FILE_ENV);

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_for_loader = Arc::clone(&calls);
        let source = EvalOpsAccessTokenSource::with_loader(move || {
            let call = calls_for_loader.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Some(format!("snapshot-token-{call}")))
        });
        assert_eq!(source.resolve().unwrap().as_str(), "snapshot-token-0");
        source.invalidate();
        assert_eq!(source.resolve().unwrap().as_str(), "snapshot-token-1");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn no_provider_login_or_refresh_is_called_by_the_source() {
        let _guard = crate::config::test_process_env_lock();
        let _restore = EnvRestore::capture(&[ACCESS_TOKEN_ENV, ACCESS_TOKEN_FILE_ENV]);
        std::env::remove_var(ACCESS_TOKEN_ENV);
        std::env::remove_var(ACCESS_TOKEN_FILE_ENV);
        let source = EvalOpsAccessTokenSource::with_loader(|| Ok(None));
        let error = source.resolve().unwrap_err();
        assert!(error.to_string().contains("cached EvalOps session"));
    }

    #[test]
    fn stored_snapshot_access_rejects_expiry_with_an_actionable_login_error() {
        assert_eq!(
            stored_snapshot_access("access-token", 2_000, 1_999).unwrap(),
            Some("access-token".to_string())
        );
        let error = stored_snapshot_access("access-token", 2_000, 2_000).unwrap_err();
        assert!(error.contains("deixic-code evalops login"));
    }

    #[test]
    fn invalidation_evicts_the_process_evalops_snapshot_cache() {
        let _guard = crate::config::test_process_env_lock();
        let _cache_reset = EvalOpsCredentialCacheReset;
        let _restore = EnvRestore::capture(&[
            "MAESTRO_HOME",
            "MAESTRO_OAUTH_STORAGE_MODE",
            "MAESTRO_DISABLE_KEYCHAIN",
            ACCESS_TOKEN_ENV,
            ACCESS_TOKEN_FILE_ENV,
        ]);
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("MAESTRO_HOME", home.path());
        std::env::set_var("MAESTRO_OAUTH_STORAGE_MODE", "file");
        std::env::set_var("MAESTRO_DISABLE_KEYCHAIN", "1");
        std::env::remove_var(ACCESS_TOKEN_ENV);
        std::env::remove_var(ACCESS_TOKEN_FILE_ENV);
        crate::init_cli::invalidate_evalops_credentials_cache();

        std::fs::write(
            home.path().join("oauth.json"),
            serde_json::json!({
                "evalops": {
                    "type": "oauth",
                    "refresh": "refresh-one",
                    "access": "access-one",
                    "expires": 9_223_372_036_854_775_807_i64
                }
            })
            .to_string(),
        )
        .unwrap();
        let auth = ManagedMcpAuth {
            access_token: EvalOpsAccessTokenSource::new(),
            #[cfg(test)]
            binding_validator: None,
        };
        assert_eq!(auth.access_token.resolve().unwrap().as_str(), "access-one");

        std::fs::write(
            home.path().join("oauth.json"),
            serde_json::json!({
                "evalops": {
                    "type": "oauth",
                    "refresh": "refresh-two",
                    "access": "access-two",
                    "expires": 9_223_372_036_854_775_807_i64
                }
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(auth.access_token.resolve().unwrap().as_str(), "access-one");

        auth.invalidate();
        assert_eq!(auth.access_token.resolve().unwrap().as_str(), "access-two");
    }
}
