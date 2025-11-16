use anyhow::Result;
use std::time::Duration;
use tracing::{debug, warn};

/// Execute an async operation with up to 3 attempts and exponential backoff (0.5s, 1s, 2s).
/// Logs failures only at debug level until the final attempt, then warns.
/// Returns the first successful result or the last error.
pub async fn retry3_quiet<F, Fut, T>(op_name: &str, mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut attempt: u32 = 0;
    let mut last_err: Option<anyhow::Error> = None;
    while attempt < 3 {
        attempt += 1;
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                last_err = Some(e);
                if attempt < 3 {
                    debug!(attempt, op=%op_name, err=?last_err, "transient failure; will retry");
                    let backoff = match attempt {
                        1 => Duration::from_millis(500),
                        2 => Duration::from_millis(1000),
                        _ => Duration::from_millis(0),
                    };
                    if backoff.as_millis() > 0 {
                        tokio::time::sleep(backoff).await;
                    }
                    continue;
                } else {
                    warn!(attempt, op=%op_name, err=?last_err, "operation failed after retries");
                }
            }
        }
        break;
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("unknown error")))
}

#[cfg(test)]
mod tests {
    use super::retry3_quiet;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn succeeds_first_try() {
        let calls = Arc::new(Mutex::new(0));
        let res: anyhow::Result<i32> = retry3_quiet("success_first", || {
            let calls = calls.clone();
            async move {
                *calls.lock().unwrap() += 1;
                Ok(42)
            }
        })
        .await;
        assert_eq!(res.unwrap(), 42);
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn succeeds_third_try() {
        let calls = Arc::new(Mutex::new(0));
        let res: anyhow::Result<i32> = retry3_quiet("success_third", || {
            let calls = calls.clone();
            async move {
                let mut g = calls.lock().unwrap();
                *g += 1;
                if *g < 3 {
                    anyhow::bail!("not yet");
                }
                Ok(7)
            }
        })
        .await;
        assert_eq!(res.unwrap(), 7);
        assert_eq!(*calls.lock().unwrap(), 3);
    }

    #[tokio::test]
    async fn fails_after_three() {
        let calls = Arc::new(Mutex::new(0));
        let res: anyhow::Result<i32> = retry3_quiet("fail", || {
            let calls = calls.clone();
            async move {
                *calls.lock().unwrap() += 1;
                anyhow::bail!("always")
            }
        })
        .await;
        assert!(res.is_err());
        assert_eq!(*calls.lock().unwrap(), 3);
    }
}
