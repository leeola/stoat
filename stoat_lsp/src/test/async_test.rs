//! Async test helpers using smol runtime.
//!
//! Provides utilities for writing async tests that work with smol-based code.

use std::future::Future;

/// Run an async test using smol executor.
///
/// This helper runs async tests with the smol runtime, matching the runtime
/// used by LspManager and other production code. This avoids async runtime
/// mismatches that occur when using #[tokio::test] with smol-based code.
///
/// # Example
///
/// ```rust,ignore
/// #[test]
/// fn my_test() {
///     run_async_test(async {
///         let manager = LspManager::new();
///         // ... async test code
///     });
/// }
/// ```
pub fn run_async_test<F, Fut>(test: F)
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = ()>,
{
    smol::block_on(test())
}

/// Run an async test with a timeout.
///
/// Fails the test if it doesn't complete within the specified duration.
pub fn run_async_test_with_timeout<F, Fut>(
    timeout: std::time::Duration,
    test: F,
) -> Result<(), &'static str>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = ()>,
{
    smol::block_on(async {
        smol::future::or(
            async {
                smol::Timer::after(timeout).await;
                Err("Test timed out")
            },
            async {
                test().await;
                Ok(())
            },
        )
        .await
    })
}
