//! Native credential access shared by OAuth, connections, and MCP.
//!
//! Unit tests and opt-in test-support dependency builds cannot open the
//! developer's credential store. Other process-based fixtures use
//! MAESTRO_DISABLE_KEYCHAIN=1 to enforce the same boundary.
use anyhow::{Result, bail};

pub(crate) fn entry(service: &str, account: &str) -> Result<keyring::Entry> {
    let disabled = std::env::var("MAESTRO_DISABLE_KEYCHAIN").ok();
    open_with_policy(
        cfg!(any(test, feature = "test-support")),
        disabled.as_deref(),
        || keyring::Entry::new(service, account).map_err(Into::into),
    )
}

fn open_with_policy<T>(
    unit_test: bool,
    disabled: Option<&str>,
    open: impl FnOnce() -> Result<T>,
) -> Result<T> {
    if unit_test {
        bail!(
            "native credential access is disabled in unit tests and test-support builds; inject a test secret backend"
        );
    }
    if disabled == Some("1") {
        bail!("native credential access is disabled by MAESTRO_DISABLE_KEYCHAIN=1");
    }
    open()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_credentials_unit_tests_cannot_open_native_entries() {
        let error = match entry("maestro-test-do-not-create", "nonexistent") {
            Ok(_) => panic!("unit test opened the native credential store"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("disabled in unit tests"));
    }

    #[test]
    fn native_credentials_disabled_paths_never_call_the_backend() {
        for (unit_test, disabled) in [(true, None), (true, Some("0")), (false, Some("1"))] {
            let result: Result<()> = open_with_policy(unit_test, disabled, || {
                panic!("disabled native credential backend was invoked")
            });
            assert!(result.is_err());
        }
    }

    #[test]
    fn native_credentials_normal_runtime_preserves_results_and_errors() {
        for disabled in [None, Some("0")] {
            assert_eq!(open_with_policy(false, disabled, || Ok(42)).unwrap(), 42);
            let error =
                open_with_policy::<()>(false, disabled, || bail!("backend failure")).unwrap_err();
            assert_eq!(error.to_string(), "backend failure");
        }
    }
}
