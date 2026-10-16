use std::io;
use std::panic;
use std::sync::Mutex;
use std::thread;

/// Applies `work` to every item on at most `max_threads` named worker threads and
/// returns the results in input order. Worker panics are re-raised in the caller.
pub(crate) fn map_bounded<T, R, F>(
    items: Vec<T>,
    max_threads: usize,
    thread_prefix: &str,
    work: F,
) -> io::Result<Vec<R>>
where
    T: Send,
    R: Send,
    F: Fn(T) -> R + Sync,
{
    let item_count = items.len();
    if item_count == 0 {
        return Ok(Vec::new());
    }
    let thread_count = max_threads.clamp(1, item_count);
    let queue = Mutex::new(items.into_iter().enumerate());
    let worker = || {
        let mut results = Vec::new();
        loop {
            // Take the next item, then release the queue before running the work.
            let next = queue
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .next();
            let Some((index, item)) = next else {
                return results;
            };
            results.push((index, work(item)));
        }
    };

    let mut indexed = thread::scope(|scope| {
        let mut handles = Vec::with_capacity(thread_count);
        for index in 0..thread_count {
            match thread::Builder::new()
                .name(format!("{thread_prefix}-{index}"))
                .spawn_scoped(scope, worker)
            {
                Ok(handle) => handles.push(handle),
                // Workers already running drain the shared queue, so only fail when none started.
                Err(error) if handles.is_empty() => return Err(error),
                Err(_) => break,
            }
        }
        let mut indexed = Vec::with_capacity(item_count);
        for handle in handles {
            match handle.join() {
                Ok(mut results) => indexed.append(&mut results),
                Err(payload) => panic::resume_unwind(payload),
            }
        }
        Ok(indexed)
    })?;
    indexed.sort_unstable_by_key(|(index, _)| *index);
    Ok(indexed.into_iter().map(|(_, result)| result).collect())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    #[test]
    fn preserves_input_order() {
        let items: Vec<u64> = (0..64).collect();
        let results = map_bounded(items, 8, "test-order", |item| {
            thread::sleep(Duration::from_micros((64 - item) * 50));
            item * 2
        })
        .unwrap();
        assert_eq!(results, (0..64).map(|item| item * 2).collect::<Vec<_>>());
    }

    #[test]
    fn never_exceeds_thread_limit() {
        let running = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        map_bounded((0..32).collect(), 3, "test-limit", |_: i32| {
            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(2));
            running.fetch_sub(1, Ordering::SeqCst);
        })
        .unwrap();
        assert!(peak.load(Ordering::SeqCst) <= 3);
    }

    #[test]
    fn names_worker_threads() {
        let names = map_bounded(vec![(); 4], 0, "test-name", |()| {
            thread::current().name().map(str::to_owned)
        })
        .unwrap();
        assert!(
            names
                .iter()
                .all(|name| name.as_deref() == Some("test-name-0"))
        );
    }

    #[test]
    fn handles_empty_input() {
        let results: Vec<()> = map_bounded(Vec::<()>::new(), 4, "test-empty", |()| ()).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    #[should_panic(expected = "worker failed")]
    fn propagates_worker_panics() {
        let _ = map_bounded(vec![1, 2, 3], 2, "test-panic", |item: i32| {
            if item == 2 {
                panic!("worker failed");
            }
            item
        });
    }
}
