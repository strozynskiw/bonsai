//! Process-global environment isolation for tests.
//!
//! This safe wrapper delegates serialization and RAII restoration to `temp-env`,
//! including its non-poisoning process-global lock. Tests must keep all
//! environment-dependent work inside one of these closures.

use std::ffi::OsStr;
use std::hash::Hash;

/// Set an environment variable for one synchronous test closure.
///
/// The prior `OsString` value is restored even when `closure` panics.
pub(crate) fn with_var<K, V, F, R>(key: K, value: Option<V>, closure: F) -> R
where
    K: AsRef<OsStr> + Clone + Eq + Hash,
    V: AsRef<OsStr> + Clone,
    F: FnOnce() -> R,
{
    temp_env::with_var(key, value, closure)
}

/// Set an environment variable for one asynchronous test future.
///
/// The process-global lock remains held until the future completes and the prior
/// `OsString` value is restored before the lock releases.
pub(crate) async fn with_var_async<K, V, F, R>(key: K, value: Option<V>, future: F) -> R
where
    K: AsRef<OsStr> + Clone + Eq + Hash,
    V: AsRef<OsStr> + Clone,
    F: Future<Output = R> + IntoFuture<Output = R>,
{
    temp_env::async_with_vars([(key, value)], future).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restores_prior_values_after_nested_scope() {
        let name = "BONSAI_TEST_ENV_GUARD_RESTORE";
        with_var(name, Some("original"), || {
            with_var(name, Some("temporary"), || {
                assert_eq!(std::env::var(name).as_deref(), Ok("temporary"));
            });
            assert_eq!(std::env::var(name).as_deref(), Ok("original"));
        });
        assert!(std::env::var(name).is_err());
    }

    #[tokio::test]
    async fn restores_values_after_async_scope() {
        let name = "BONSAI_TEST_ENV_GUARD_ASYNC";
        with_var_async(name, Some("temporary"), async {
            assert_eq!(std::env::var(name).as_deref(), Ok("temporary"));
        })
        .await;
        assert!(std::env::var(name).is_err());
    }
}
