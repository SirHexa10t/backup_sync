//! Run a batch of independent tasks either sequentially or across a bounded thread pool.
//!
//! `jobs <= 1` runs the work **directly** — a plain iterator, no thread pool, no rayon scheduling
//! overhead (the default). `jobs > 1` runs it in a freshly-built pool of exactly `jobs` threads.
//! Input order is preserved either way, so results line up with their inputs.

use rayon::prelude::*;

/// Map `f` over `items` — sequentially when `jobs <= 1`, else across `jobs` worker threads.
pub fn map<T, O, F>(jobs: usize, items: Vec<T>, f: F) -> Vec<O>
where
    T: Send,
    O: Send,
    F: Fn(T) -> O + Sync + Send,
{
    if jobs <= 1 {
        return items.into_iter().map(f).collect();
    }
    match rayon::ThreadPoolBuilder::new().num_threads(jobs).build() {
        Ok(pool) => pool.install(|| items.into_par_iter().map(&f).collect::<Vec<O>>()),
        Err(_) => items.into_iter().map(f).collect(), // couldn't build a pool → just run it directly
    }
}
