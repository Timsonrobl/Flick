//! Background decoding of `Screen` levels for the preload ring
//! (ARCHITECTURE.md §6). The queue is replaced wholesale on every
//! navigation, newest wish first, so fast wheeling never builds a backlog:
//! images scrolled past are dropped from the queue before they start.
//! A decode that has already started runs to completion; its result is
//! simply not used if nobody wants it any more. The picture a window is
//! headed for never waits behind those: `urgent` starts it at once on a
//! thread of its own (preloads of pictures scrolled past can hold every
//! worker for a heavy file's whole second). Preloads stop short of a RAW's own
//! decode (`decode::decode_preload`): only the urgent picture gets it.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use crate::decode::{self, Picture};

type Done = dyn Fn(PathBuf, Result<Picture, String>, f64) + Send + Sync;

struct Queue {
    waiting: VecDeque<PathBuf>,
    in_flight: HashSet<PathBuf>,
    /// The last picture asked for as urgent.
    latest: Option<PathBuf>,
    /// Urgent previews under way.
    previewing: HashSet<PathBuf>,
}

struct Shared {
    queue: Mutex<Queue>,
    wake: Condvar,
}

pub struct Pool {
    shared: Arc<Shared>,
    target: (u32, u32),
    done: Arc<Done>,
}

impl Pool {
    /// `done(path, picture, decode_ms)` is called on a worker thread.
    pub fn new(threads: usize, target: (u32, u32), done: impl Fn(PathBuf, Result<Picture, String>, f64) + Send + Sync + 'static) -> Pool {
        let shared = Arc::new(Shared { queue: Mutex::new(Queue { waiting: VecDeque::new(), in_flight: HashSet::new(), latest: None, previewing: HashSet::new() }), wake: Condvar::new() });
        let done: Arc<Done> = Arc::new(done);
        for _ in 0..threads {
            let (shared, done) = (shared.clone(), done.clone());
            std::thread::spawn(move || worker(&shared, target, &*done));
        }
        Pool { shared, target, done }
    }

    /// A picture paging passes: its quick preview only, now, on a thread of
    /// its own (a progressive JPEG's first scans: ~90 ms; nothing for other
    /// formats). Unless it is previewing already.
    pub fn preview(&self, path: PathBuf) {
        if !self.shared.queue.lock().unwrap_or_else(|e| e.into_inner()).previewing.insert(path.clone()) {
            return;
        }
        let (shared, done) = (self.shared.clone(), self.done.clone());
        std::thread::spawn(move || {
            let started = Instant::now();
            let picture = decode::decode_preview(&path);
            shared.queue.lock().unwrap_or_else(|e| e.into_inner()).previewing.remove(&path);
            if let Some(picture) = picture {
                done(path, Ok(picture), started.elapsed().as_secs_f64() * 1000.0);
            }
        });
    }

    /// Decodes `path` now, on a thread of its own at normal priority, unless
    /// it is decoding already: first a quick preview if `preview` and the
    /// format has one (a progressive JPEG: `decode::decode_preview`), then
    /// the screen level, unless a newer urgent picture was asked for
    /// meanwhile (a fast wheel would otherwise start a full decode per
    /// notch, each slowing the one that matters).
    pub fn urgent(&self, path: PathBuf, preview: bool) {
        let mut queue = self.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.latest = Some(path.clone());
        // Its full decode may be under way already (a preload): the preview still runs.
        let full = queue.in_flight.insert(path.clone());
        let preview = preview && queue.previewing.insert(path.clone());
        if !full && !preview {
            return;
        }
        queue.waiting.retain(|p| *p != path);
        drop(queue);
        let (shared, done, target) = (self.shared.clone(), self.done.clone(), self.target);
        std::thread::spawn(move || {
            if preview {
                let started = Instant::now();
                let picture = decode::decode_preview(&path);
                let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
                queue.previewing.remove(&path);
                let passed = queue.latest.as_ref() != Some(&path);
                if full && passed && picture.is_some() {
                    queue.in_flight.remove(&path); // passed: its preview is enough
                }
                drop(queue);
                if let Some(picture) = picture {
                    done(path.clone(), Ok(picture), started.elapsed().as_secs_f64() * 1000.0);
                    if passed {
                        return;
                    }
                }
            }
            if full {
                decode_one(&shared, path, target, false, &*done);
            }
        });
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
        decode_one(shared, path, target, true, done);
    }
}

/// `preload`: for the ring, not a picture a window heads for (`decode::decode_preload`).
fn decode_one(shared: &Shared, path: PathBuf, target: (u32, u32), preload: bool, done: &Done) {
    let started = Instant::now();
    let picture = if preload { decode::decode_preload(&path, target) } else { decode::decode_screen(&path, target) };
    let ms = started.elapsed().as_secs_f64() * 1000.0;
    shared.queue.lock().unwrap_or_else(|e| e.into_inner()).in_flight.remove(&path);
    done(path, picture, ms);
}
