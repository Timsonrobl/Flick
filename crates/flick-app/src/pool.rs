//! Background decoding of `Screen` levels for the preload ring
//! (ARCHITECTURE.md §6). The queue is replaced wholesale on every
//! navigation, newest wish first, so fast wheeling never builds a backlog:
//! images scrolled past are dropped from the queue before they start.
//! A decode that has already started runs to completion; its result is
//! simply not used if nobody wants it any more.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use crate::decode::{self, Picture};

type Done = dyn Fn(PathBuf, Result<Picture, String>, f64) + Send + Sync;

struct Queue {
    waiting: VecDeque<PathBuf>,
    in_flight: HashSet<PathBuf>,
}

struct Shared {
    queue: Mutex<Queue>,
    wake: Condvar,
}

pub struct Pool {
    shared: Arc<Shared>,
}

impl Pool {
    /// `done(path, picture, decode_ms)` is called on a worker thread.
    pub fn new(threads: usize, target: (u32, u32), done: impl Fn(PathBuf, Result<Picture, String>, f64) + Send + Sync + 'static) -> Pool {
        let shared = Arc::new(Shared { queue: Mutex::new(Queue { waiting: VecDeque::new(), in_flight: HashSet::new() }), wake: Condvar::new() });
        let done: Arc<Done> = Arc::new(done);
        for _ in 0..threads {
            let (shared, done) = (shared.clone(), done.clone());
            std::thread::spawn(move || worker(&shared, target, &*done));
        }
        Pool { shared }
    }

    /// Replaces everything not yet started with `paths`, in priority order.
    pub fn set_queue(&self, paths: Vec<PathBuf>) {
        let mut queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.waiting = paths.into_iter().filter(|p| !queue.in_flight.contains(p)).collect();
        drop(queue);
        self.shared.wake.notify_all();
    }

    pub fn is_busy_with(&self, path: &PathBuf) -> bool {
        let queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.in_flight.contains(path) || queue.waiting.contains(path)
    }
}

fn worker(shared: &Shared, target: (u32, u32), done: &Done) {
    // Preloading must never compete with drawing or the compositor.
    unsafe {
        use windows::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
    loop {
        let path = {
            let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(path) = queue.waiting.pop_front() {
                    queue.in_flight.insert(path.clone());
                    break path;
                }
                queue = shared.wake.wait(queue).unwrap_or_else(|e| e.into_inner());
            }
        };
        let started = Instant::now();
        let picture = decode::decode_screen(&path, target);
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        shared.queue.lock().unwrap_or_else(|e| e.into_inner()).in_flight.remove(&path);
        done(path, picture, ms);
    }
}
