//! Port of `server/scraper/pool.ts`: runs `fetch` for up to `concurrency` items at a time, but hands the results to
//! `commit` strictly in input order, so the database ends up exactly as a one-at-a-time loop would leave it.

use crate::riot::rate_limiter::{Aborted, Signal};
use anyhow::Result;
use std::future::Future;

pub async fn in_order<T: Clone, R, F, Fut>(items: &[T], concurrency: usize, signal: &Signal, fetch: F, mut commit: impl FnMut(R, &T, usize) -> Result<bool>) -> Result<()>
where
    F: Fn(T, usize, Signal) -> Fut,
    Fut: Future<Output = Result<R>>,
{
    use futures_util::stream::{FuturesOrdered, StreamExt};
    let stop = Signal::new();
    let linked = Signal::any(&[signal, &stop]);
    let width = concurrency.max(1);
    let mut started = FuturesOrdered::new();
    let mut launched = 0usize;
    let mut result: Result<()> = Ok(());
    for i in 0..items.len() {
        if let Err(e) = signal.check() {
            result = Err(e.into());
            break;
        }
        while launched < items.len() && launched - i < width {
            started.push_back(fetch(items[launched].clone(), launched, linked.clone()));
            launched += 1;
        }
        let r = started.next().await.expect("started");
        if let Err(e) = signal.check() {
            result = Err(e.into());
            break;
        }
        match r.and_then(|r| commit(r, &items[i], i)) {
            Ok(true) => {}
            Ok(false) => break,
            Err(e) => {
                result = Err(e);
                break;
            }
        }
    }
    if !started.is_empty() {
        stop.abort("stopped");
        while started.next().await.is_some() {}
    }
    result
}

#[allow(dead_code)]
fn _a(_: Aborted) {}

#[cfg(test)]
mod tests {
    //! Port of server/scraper/pool.test.ts.
    use super::*;
    use parking_lot::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    async fn later<T>(ms: u64, value: T, signal: Option<&Signal>) -> Result<T> {
        match signal {
            Some(s) => tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(ms)) => Ok(value),
                _ = s.cancelled() => Err(anyhow::anyhow!(Aborted(s.reason().unwrap_or_default()))),
            },
            None => {
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                Ok(value)
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn results_are_committed_in_input_order() {
        let delays = vec![30u64, 5, 20, 1, 10, 2];
        let committed = Mutex::new(vec![]);
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        in_order(&delays, 3, &Signal::new(), |ms, i, _| {
            let (r, p) = (running.clone(), peak.clone());
            
            async move {
                let n = r.fetch_add(1, Ordering::SeqCst) + 1;
                p.fetch_max(n, Ordering::SeqCst);
                let v = later(ms, i, None).await;
                r.fetch_sub(1, Ordering::SeqCst);
                v
            }
        }, |v, _, i| {
            assert_eq!(v, i);
            committed.lock().push(i);
            Ok(true)
        })
        .await
        .unwrap();
        assert_eq!(*committed.lock(), vec![0, 1, 2, 3, 4, 5]);
        assert_eq!(peak.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_skips_and_aborts_the_outstanding_ones() {
        let started = Arc::new(Mutex::new(vec![]));
        let aborted = Arc::new(Mutex::new(vec![]));
        let committed = Mutex::new(vec![]);
        in_order(&[0, 1, 2, 3, 4, 5, 6, 7], 3, &Signal::new(), |i, _, signal| {
            let (st, ab) = (started.clone(), aborted.clone());
            async move {
                st.lock().push(i);
                let r = later(if i == 2 { 1 } else { 50 }, i, Some(&signal)).await;
                if r.is_err() {
                    ab.lock().push(i);
                }
                r
            }
        }, |i, _, _| {
            committed.lock().push(i);
            Ok(i < 2)
        })
        .await
        .unwrap();
        assert_eq!(*committed.lock(), vec![0, 1, 2]);
        assert!(!started.lock().contains(&7));
        let a = aborted.lock().clone();
        assert!(!a.is_empty() && a.iter().all(|i| *i > 2), "{a:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn errors_surface_in_order_and_cancellation_propagates() {
        let committed = Mutex::new(vec![]);
        let err = in_order(&[0, 1, 2, 3], 3, &Signal::new(), |i, _, _| {
            async move {
                if i == 1 {
                    anyhow::bail!("item 1 failed")
                }
                if i == 2 {
                    anyhow::bail!("item 2 failed")
                }
                later(5, i, None).await
            }
        }, |i, _, _| {
            committed.lock().push(i);
            Ok(true)
        })
        .await;
        assert_eq!(err.unwrap_err().to_string(), "item 1 failed");
        assert_eq!(*committed.lock(), vec![0]);
        let cancel = Signal::new();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            c2.abort("scan cancelled");
        });
        let err = in_order(&[0, 1, 2, 3, 4], 2, &cancel, |i, _, s| {
            async move { later(15, i, Some(&s)).await }
        }, |_, _, _| Ok(true))
        .await;
        assert!(err.unwrap_err().to_string().contains("scan cancelled"));
    }
}
