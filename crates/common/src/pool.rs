//! Adaptive page-parallel map with independently opened worker handles.
//!
//! Pages run one by one on the caller's handle. Once the run has taken longer than
//! [`PoolTuning::switch_after`] and at least [`PoolTuning::min_pages`] pages remain, the rest
//! go to up to [`PoolTuning::max_workers`] threads, each with a handle of its own, in sixteen
//! chunks per worker. Results come back in page order; when chunks fail, the error of the
//! first failing chunk in page order wins.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::error::GoatError;
use crate::py::{PyInt, parse_int};

/// One page task: what a worker opens, and what it computes for one page.
pub trait PageTask: Sync {
    /// A worker's own handle: the open document and whatever else the task reads.
    type Doc;
    /// One page's result.
    type Value: Send;

    /// Opens a handle. Every pool worker opens its own; the sequential start uses the
    /// caller's.
    fn open(&self) -> Result<Self::Doc, GoatError>;

    /// Computes one page, `index` 0-based.
    fn page(&self, doc: &mut Self::Doc, index: usize) -> Result<Self::Value, GoatError>;
}

/// When the map switches to threads, and how many it uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolTuning {
    /// Most threads the pool starts; 1 or fewer keeps every run sequential.
    pub max_workers: usize,
    /// Fewest pages that must remain for the pool to start.
    pub min_pages: usize,
    /// Sequential time before the pool starts. Zero starts it after the first page;
    /// `Duration::MAX` never starts it.
    pub switch_after: Duration,
}

/// Default worker ceiling before the CPU cap, as `PDF_GOAT_WORKERS` documents it.
const DEFAULT_WORKERS: i64 = 8;

impl PoolTuning {
    /// Defaults: 200 ms, 8 pages, and `min(workers, CPU count)` threads, where
    /// `workers` is the `PDF_GOAT_WORKERS` value (`None` when unset, meaning 8).
    ///
    /// A value `int()` rejects fails with Python's `ValueError`.
    pub fn from_setting(workers: Option<&str>) -> Result<Self, GoatError> {
        let requested = match workers {
            Some(text) => parse_int(text)?,
            None => PyInt::Small(DEFAULT_WORKERS),
        };
        let cpus = thread::available_parallelism().map_or(1, usize::from);
        let max_workers = match &requested {
            PyInt::Small(value) => usize::try_from(*value).map_or(0, |value| value.min(cpus)),
            PyInt::Huge(_) if requested.is_negative() => 0,
            PyInt::Huge(_) => cpus,
        };
        Ok(Self {
            max_workers,
            min_pages: 8,
            switch_after: Duration::from_millis(200),
        })
    }

    fn switch_now(&self, remaining: usize, started: Instant) -> bool {
        self.max_workers > 1
            && remaining >= self.min_pages
            && (self.switch_after.is_zero() || started.elapsed() > self.switch_after)
    }
}

/// Where a map saves what it computed, as it goes.
pub(crate) trait Store<V>: Sync {
    /// A saving handle of one thread's own.
    type Handle;

    /// Opens a pool worker's handle.
    fn open(&self) -> Self::Handle;

    /// Saves computed pages; `indices` and `values` pair up.
    fn save(&self, handle: &Self::Handle, indices: &[usize], values: &[V]);
}

/// A map that saves nothing.
pub(crate) struct Discard;

impl<V> Store<V> for Discard {
    type Handle = ();

    fn open(&self) {}

    fn save(&self, (): &(), _indices: &[usize], _values: &[V]) {}
}

/// `map_pages(doc, src, task, indices)`: every index's value, in the order given.
pub fn map_pages<T: PageTask>(
    task: &T,
    doc: &mut T::Doc,
    indices: &[usize],
    tuning: &PoolTuning,
) -> Result<Vec<T::Value>, GoatError> {
    map_with_store(task, doc, indices, tuning, &Discard, &())
}

pub(crate) fn map_with_store<T: PageTask, S: Store<T::Value>>(
    task: &T,
    doc: &mut T::Doc,
    indices: &[usize],
    tuning: &PoolTuning,
    store: &S,
    handle: &S::Handle,
) -> Result<Vec<T::Value>, GoatError> {
    let started = Instant::now();
    let mut values = Vec::with_capacity(indices.len());
    for (position, &index) in indices.iter().enumerate() {
        values.push(task.page(doc, index)?);
        if tuning.switch_now(indices.len() - position - 1, started) {
            store.save(handle, &indices[..=position], &values);
            let rest = &indices[position + 1..];
            values.extend(map_in_pool(task, doc, rest, tuning, store, handle)?);
            return Ok(values);
        }
    }
    store.save(handle, indices, &values);
    Ok(values)
}

/// `_map_in_pool`: `indices` across worker threads, results in order.
fn map_in_pool<T: PageTask, S: Store<T::Value>>(
    task: &T,
    doc: &mut T::Doc,
    indices: &[usize],
    tuning: &PoolTuning,
    store: &S,
    handle: &S::Handle,
) -> Result<Vec<T::Value>, GoatError> {
    if indices.is_empty() {
        return Ok(Vec::new());
    }
    let workers = tuning.max_workers.min(indices.len());
    // Sixteen chunks per worker: page cost varies a lot for rendering, and coarse chunks
    // leave the last worker alone with the heaviest pages.
    let slices = workers * 16;
    let edges: Vec<usize> = (0..=slices)
        .map(|slice| indices.len() * slice / slices)
        .collect();
    let chunks: Vec<&[usize]> = edges
        .windows(2)
        .filter(|pair| pair[0] < pair[1])
        .map(|pair| &indices[pair[0]..pair[1]])
        .collect();
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let mut slots: Vec<Option<Result<Vec<T::Value>, GoatError>>> =
        chunks.iter().map(|_| None).collect();
    thread::scope(|scope| {
        let mut running = Vec::with_capacity(workers);
        for _ in 0..workers {
            let spawned = thread::Builder::new()
                .spawn_scoped(scope, || work(task, store, &chunks, &next, &stop));
            // A thread the host cannot start leaves its chunks to the others.
            if let Ok(worker) = spawned {
                running.push(worker);
            }
        }
        for worker in running {
            match worker.join() {
                Ok(done) => {
                    for (chunk, result) in done {
                        slots[chunk] = Some(result);
                    }
                }
                Err(panic) => std::panic::resume_unwind(panic),
            }
        }
    });
    let mut values = Vec::with_capacity(indices.len());
    let mut announced = false;
    for (slot, chunk) in slots.into_iter().zip(&chunks) {
        match slot {
            Some(result) => values.extend(result?),
            None => {
                // Chunks are claimed in order and a claimed chunk always finishes, so an
                // unclaimed chunk before any failure means no worker could run.
                if !announced {
                    eprintln!("pdf-goat: worker pool unavailable; finishing in one process");
                    announced = true;
                }
                let computed = run_chunk(task, doc, chunk)?;
                store.save(handle, chunk, &computed);
                values.extend(computed);
            }
        }
    }
    Ok(values)
}

/// Finished chunks, each with its position in the claim order.
type ChunkResults<V> = Vec<(usize, Result<Vec<V>, GoatError>)>;

/// One worker thread: claims chunks in order until none remain or one has failed.
fn work<T: PageTask, S: Store<T::Value>>(
    task: &T,
    store: &S,
    chunks: &[&[usize]],
    next: &AtomicUsize,
    stop: &AtomicBool,
) -> ChunkResults<T::Value> {
    let mut done = Vec::new();
    // A worker that cannot open its own handle leaves its chunks to the others.
    let Ok(mut doc) = task.open() else {
        return done;
    };
    let handle = store.open();
    while !stop.load(Ordering::Acquire) {
        let claimed = next.fetch_add(1, Ordering::AcqRel);
        let Some(chunk) = chunks.get(claimed) else {
            break;
        };
        let result = run_chunk(task, &mut doc, chunk);
        match &result {
            Ok(values) => store.save(&handle, chunk, values),
            Err(_) => stop.store(true, Ordering::Release),
        }
        done.push((claimed, result));
    }
    done
}

fn run_chunk<T: PageTask>(
    task: &T,
    doc: &mut T::Doc,
    chunk: &[usize],
) -> Result<Vec<T::Value>, GoatError> {
    chunk.iter().map(|&index| task.page(doc, index)).collect()
}
