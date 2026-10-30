//! The end caller's API key for the current request, for the cloud-platform
//! reads that answer only for that key (model access). The API-key middleware
//! puts it in scope only when cloud-platform is configured. It stays in memory
//! for the request and is never logged.
//!
//! A task-local does not follow `tokio::spawn`: work spawned off a request
//! must capture the key first and re-enter the scope, as
//! [`spawn_with_caller_credential`] does.

use std::future::Future;

tokio::task_local! {
    static CALLER_CREDENTIAL: String;
}

/// Run `fut` with the end caller's API key in scope.
pub async fn with_caller_credential<F: Future>(token: String, fut: F) -> F::Output {
    CALLER_CREDENTIAL.scope(token, fut).await
}

/// The end caller's API key in scope, if the request carries one.
pub fn current_caller_credential() -> Option<String> {
    CALLER_CREDENTIAL.try_with(|token| token.clone()).ok()
}

/// `tokio::spawn`, carrying the caller's key (if any is in scope) into the
/// spawned task. With no key in scope this is a plain spawn.
pub fn spawn_with_caller_credential<F>(fut: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    match current_caller_credential() {
        Some(token) => tokio::spawn(with_caller_credential(token, fut)),
        None => tokio::spawn(fut),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_spawned_task_sees_the_callers_key_only_when_carried() {
        let (carried, plain) = with_caller_credential("sk-caller".to_string(), async {
            (
                spawn_with_caller_credential(async { current_caller_credential() })
                    .await
                    .unwrap(),
                tokio::spawn(async { current_caller_credential() })
                    .await
                    .unwrap(),
            )
        })
        .await;
        assert_eq!(carried.as_deref(), Some("sk-caller"));
        // Why the helper exists: a plain spawn drops the task-local.
        assert_eq!(plain, None);
    }

    #[tokio::test]
    async fn without_a_key_in_scope_the_spawned_task_has_none() {
        let seen = spawn_with_caller_credential(async { current_caller_credential() })
            .await
            .unwrap();
        assert_eq!(seen, None);
    }
}
