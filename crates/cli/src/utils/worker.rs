use crate::print::Printer;
use crate::utils::FileTrace;

use anyhow::{Result, anyhow};
use ignore::{DirEntry, WalkParallel, WalkState};

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};

/// A trait to abstract how vorpal discovers work Items.
///
/// It follows multiple-producer-single-consumer pattern.
/// vorpal will produce items in one or more separate thread(s) and
/// `consume_items` in the main thread, blocking the function return.
/// Worker at the moment has two main flavors:
/// * PathWorker: discovers files on the file system, based on ignore
/// * StdInWorker: parse text content from standard input stream
pub trait Worker: Sync + Send {
  /// `consume_items` will run in a separate single thread.
  /// printing matches or error reporting can happen here.
  fn consume_items<P: Printer>(&self, items: Items<P::Processed>, printer: P) -> Result<ExitCode>;
}

/// A trait to abstract how vorpal discovers, parses and processes files.
///
/// It follows multiple-producer-single-consumer pattern.
/// vorpal discovers files in parallel by `build_walk`.
/// Then every file is parsed and filtered in `produce_item`.
/// Finally, `produce_item` will send `Item` to the consumer thread.
pub trait PathWorker: Worker {
  /// WalkParallel will determine what files will be processed.
  fn build_walk(&self) -> Result<WalkParallel>;
  /// Record trace for the worker.
  fn get_trace(&self) -> &FileTrace;
  /// Parse and find_match can be done in `produce_item`.
  fn produce_item<P: Printer>(
    &self,
    path: &Path,
    processor: &P::Processor,
  ) -> Result<Vec<P::Processed>>;

  /// Returns true if the worker should stop processing files.
  /// Used to implement early termination (e.g., --max-results).
  fn should_stop(&self) -> bool {
    false
  }

  fn run_path<P: Printer + 'static>(self, printer: P) -> Result<ExitCode>
  where
    Self: Sized + 'static,
  {
    run_worker(Arc::new(self), printer)
  }
}

pub trait StdInWorker: Worker {
  fn parse_stdin<P: Printer>(
    &self,
    src: String,
    processor: &P::Processor,
  ) -> Result<Vec<P::Processed>>;

  fn run_std_in<P: Printer>(&self, printer: P) -> Result<ExitCode> {
    let source = std::io::read_to_string(std::io::stdin())?;
    let processor = printer.get_processor();
    if let Ok(items) = self.parse_stdin::<P>(source, &processor) {
      self.consume_items(Items::once(items)?, printer)
    } else {
      // return exit code 2 on parse error
      Ok(ExitCode::from(2))
    }
  }
}

pub struct Items<T>(mpsc::Receiver<T>);
impl<T> Iterator for Items<T> {
  type Item = T;
  fn next(&mut self) -> Option<Self::Item> {
    // TODO: add error reporting here
    self.0.recv().ok()
  }
}
impl<T> Items<T> {
  fn once(items: Vec<T>) -> Result<Self> {
    let (tx, rx) = mpsc::channel();
    for item in items {
      // use write to avoid send/sync trait bound
      match tx.send(item) {
        Ok(_) => (),
        Err(e) => return Err(anyhow!(e.to_string())),
      };
    }
    Ok(Items(rx))
  }
}

/// Keep only files from a walk result, stripping one leading `./` for display. Shared with the
/// remote agent/stream drivers so display paths cannot drift between local and remote (I1).
pub(crate) fn filter_result(result: Result<DirEntry, ignore::Error>) -> Option<PathBuf> {
  let entry = match result {
    Ok(entry) => entry,
    Err(err) => {
      eprintln!("ERROR: {err}");
      return None;
    }
  };
  if !entry.file_type()?.is_file() {
    return None;
  }
  let path = entry.into_path();
  // TODO: is it correct here? see https://github.com/ast-grep/ast-grep/issues/1343
  match path.strip_prefix("./") {
    Ok(p) => Some(p.to_path_buf()),
    Err(_) => Some(path),
  }
}

/// How the producer→consumer channel is sized. The local walk keeps today's effectively-unbounded
/// channel (behavior-preserving); remote producers use a real window so the printer's drain rate
/// paces the network end-to-end and nothing buffers the whole corpus.
#[derive(Clone, Copy)]
pub enum ChannelCap {
  Unbounded,
  Window(usize),
}

/// The sending half handed to a [`Produce`] implementation — unbounded and windowed channels have
/// different sender types in std, unified here so producers are agnostic.
pub enum ItemSink<T> {
  Unbounded(mpsc::Sender<T>),
  Bounded(mpsc::SyncSender<T>),
}

impl<T> Clone for ItemSink<T> {
  fn clone(&self) -> Self {
    match self {
      ItemSink::Unbounded(tx) => ItemSink::Unbounded(tx.clone()),
      ItemSink::Bounded(tx) => ItemSink::Bounded(tx.clone()),
    }
  }
}

impl<T> ItemSink<T> {
  /// Send one item; `Err` means the consumer hung up and the producer should stop.
  pub fn send(&self, item: T) -> Result<(), mpsc::SendError<T>> {
    match self {
      ItemSink::Unbounded(tx) => tx.send(item),
      ItemSink::Bounded(tx) => tx.send(item),
    }
  }
}

/// A pluggable producer: anything that can feed `P::Processed` items into the sync channel drained
/// by [`Worker::consume_items`]. The consumer side — `Items`, every printer, and the exit-code
/// logic — is shared verbatim between local walks and remote fan-out (§3, `docs/REMOTE.md`).
pub trait Produce<P: Printer>: Send + 'static {
  /// Channel sizing; local stays unbounded, remote uses a window.
  fn channel_cap(&self) -> ChannelCap {
    ChannelCap::Unbounded
  }
  fn produce(self: Box<Self>, tx: ItemSink<P::Processed>, processor: P::Processor) -> Result<()>;
}

/// The current local producer: a parallel filesystem walk driving `produce_item`. This is the
/// pre-existing `run_worker` spawn body, moved verbatim behind [`Produce`].
pub struct LocalWalkProducer<W: PathWorker + ?Sized> {
  worker: Arc<W>,
  walker: WalkParallel,
}

impl<W: PathWorker + ?Sized> LocalWalkProducer<W> {
  /// Builds the walk up front so discovery errors surface synchronously, exactly as before.
  pub fn try_new(worker: Arc<W>) -> Result<Self> {
    let walker = worker.build_walk()?;
    Ok(Self { worker, walker })
  }
}

impl<W: PathWorker + ?Sized + 'static, P: Printer + 'static> Produce<P> for LocalWalkProducer<W> {
  fn produce(self: Box<Self>, tx: ItemSink<P::Processed>, processor: P::Processor) -> Result<()> {
    let LocalWalkProducer { worker: w, walker } = *self;
    walker.run(|| {
      let tx = tx.clone();
      let w = w.clone();
      let processor = &processor;
      Box::new(move |result| {
        let Some(p) = filter_result(result) else {
          return WalkState::Continue;
        };
        let stats = w.get_trace();
        stats.add_scanned();
        let Ok(items) = w.produce_item::<P>(&p, processor) else {
          stats.add_skipped();
          return WalkState::Continue;
        };
        for result in items {
          match tx.send(result) {
            Ok(_) => continue,
            Err(_) => return WalkState::Quit,
          }
        }
        if w.should_stop() {
          return WalkState::Quit;
        }
        WalkState::Continue
      })
    });
    Ok(())
  }
}

/// Drives any producer against the existing single-threaded consumer + printer. The producer runs
/// on its own thread (the walker / remote reactor blocks it); `consume_items` runs on the caller's
/// thread and owns all printing and exit-code decisions, untouched.
pub fn run_producer<W, P>(
  worker: Arc<W>,
  producer: Box<dyn Produce<P>>,
  printer: P,
) -> Result<ExitCode>
where
  W: Worker + ?Sized + 'static,
  P: Printer + 'static,
{
  let processor = printer.get_processor();
  let (sink, rx) = match producer.channel_cap() {
    ChannelCap::Unbounded => {
      let (tx, rx) = mpsc::channel();
      (ItemSink::Unbounded(tx), rx)
    }
    ChannelCap::Window(n) => {
      let (tx, rx) = mpsc::sync_channel(n.max(1));
      (ItemSink::Bounded(tx), rx)
    }
  };
  // producer run will block its thread (walker or remote reactor)
  std::thread::spawn(move || {
    let _ = producer.produce(sink, processor);
  });
  worker.consume_items(Items(rx), printer)
}

fn run_worker<W: PathWorker + ?Sized + 'static, P: Printer + 'static>(
  worker: Arc<W>,
  printer: P,
) -> Result<ExitCode> {
  let producer = LocalWalkProducer::try_new(worker.clone())?;
  run_producer(worker, Box::new(producer), printer)
}

pub struct MaxItemCounter(AtomicUsize);

impl MaxItemCounter {
  /// The baseline is to pack two usize (max and curr item)
  /// into one atomic usize without underflowing
  pub const BASELINE: usize = 2usize << 20;

  pub fn new(max: u16) -> Self {
    Self(AtomicUsize::new(max as usize + Self::BASELINE))
  }

  /// returning the actual reserved count
  pub fn claim(&self, count: usize) -> usize {
    let count = count.min(Self::BASELINE);
    let prev = self.0.fetch_sub(count, Ordering::AcqRel);
    prev.saturating_sub(Self::BASELINE).min(count)
  }

  pub fn reached_max(&self) -> bool {
    self.0.load(Ordering::Acquire) <= Self::BASELINE
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_max_item_counter_initialization() {
    let counter = MaxItemCounter::new(100);
    assert_eq!(
      counter.0.load(Ordering::Acquire),
      100 + MaxItemCounter::BASELINE
    );
    assert!(!counter.reached_max());
  }

  #[test]
  fn test_max_item_counter_claim_within_limit() {
    let counter = MaxItemCounter::new(10);

    // Claim 5 items, should get all 5
    let claimed = counter.claim(5);
    assert_eq!(claimed, 5);
    assert!(!counter.reached_max());

    // Claim 3 more, should get all 3
    let claimed = counter.claim(3);
    assert_eq!(claimed, 3);
    assert!(!counter.reached_max());
  }

  #[test]
  fn test_max_item_counter_claim_exceeds_limit() {
    let counter = MaxItemCounter::new(5);

    // Claim 3 items
    let claimed = counter.claim(3);
    assert_eq!(claimed, 3);

    // Try to claim 5 more, but only 2 remain
    let claimed = counter.claim(5);
    assert_eq!(claimed, 2);
    assert!(counter.reached_max());
  }

  #[test]
  fn test_max_item_counter_reached_max() {
    let counter = MaxItemCounter::new(3);

    assert!(!counter.reached_max());

    counter.claim(2);
    assert!(!counter.reached_max());

    counter.claim(1);
    assert!(counter.reached_max());

    // Additional claims should return 0
    let claimed = counter.claim(1);
    assert_eq!(claimed, 0);
    assert!(counter.reached_max());
  }

  #[test]
  fn test_max_item_counter_claim_zero() {
    let counter = MaxItemCounter::new(10);

    let claimed = counter.claim(0);
    assert_eq!(claimed, 0);
    assert!(!counter.reached_max());
  }

  #[test]
  fn test_max_item_counter_claim_more_than_baseline() {
    let counter = MaxItemCounter::new(10);

    // Try to claim more than BASELINE, should be clamped
    let huge_count = MaxItemCounter::BASELINE + 1000;
    let claimed = counter.claim(huge_count);

    // Should only claim up to the max (10)
    assert_eq!(claimed, 10);
    assert!(counter.reached_max());
  }

  #[test]
  fn test_max_item_counter_multiple_small_claims() {
    let counter = MaxItemCounter::new(10);

    for _ in 0..10 {
      let claimed = counter.claim(1);
      assert_eq!(claimed, 1);
    }

    assert!(counter.reached_max());

    // Next claim should return 0
    let claimed = counter.claim(1);
    assert_eq!(claimed, 0);
  }

  #[test]
  fn test_max_item_counter_zero_max() {
    let counter = MaxItemCounter::new(0);

    assert!(counter.reached_max());

    let claimed = counter.claim(1);
    assert_eq!(claimed, 0);
  }

  #[test]
  fn test_max_item_counter_partial_claim() {
    let counter = MaxItemCounter::new(7);

    // Claim 10, but only 7 available
    let claimed = counter.claim(10);
    assert_eq!(claimed, 7);
    assert!(counter.reached_max());
  }

  #[test]
  fn test_max_item_counter_concurrent_claims() {
    use std::thread;

    let counter = Arc::new(MaxItemCounter::new(100));
    let mut handles = vec![];

    // Spawn 10 threads, each claiming 15 items
    for _ in 0..10 {
      let counter_clone = Arc::clone(&counter);
      let handle = thread::spawn(move || counter_clone.claim(15));
      handles.push(handle);
    }

    // Collect all claimed amounts
    let total_claimed: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();

    // Total claimed should equal the max (100)
    assert_eq!(total_claimed, 100);
    assert!(counter.reached_max());
  }
}
