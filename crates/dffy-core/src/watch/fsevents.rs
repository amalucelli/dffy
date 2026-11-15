//! The macOS backend: one `FSEvents` stream per root, recursive by itself.
//! The stream covers the git directories too when they live outside the
//! working tree (linked worktrees); the coalescer drops git's internals.
//!
//! FFI, so unsafe is allowed here only. Type-checked on Linux, never run
//! there.
#![expect(unsafe_code, reason = "FFI to FSEvents and Core Foundation")]

use std::ffi::{CStr, OsStr, c_char, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::slice;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};

use objc2_core_foundation::{CFArray, CFRunLoop, CFString, kCFRunLoopDefaultMode};
#[expect(deprecated, reason = "see the call in `run`")]
use objc2_core_services::FSEventStreamScheduleWithRunLoop;
use objc2_core_services::{
    ConstFSEventStreamRef, FSEventStreamContext, FSEventStreamCreate, FSEventStreamEventFlags,
    FSEventStreamEventId, FSEventStreamInvalidate, FSEventStreamRelease, FSEventStreamStart,
    FSEventStreamStop, kFSEventStreamCreateFlagFileEvents, kFSEventStreamCreateFlagNoDefer,
    kFSEventStreamEventFlagItemIsDir, kFSEventStreamEventFlagKernelDropped,
    kFSEventStreamEventFlagMustScanSubDirs, kFSEventStreamEventFlagUserDropped,
    kFSEventStreamEventIdSinceNow,
};

use super::{Raw, Root, Shared, SharedExcludes};

pub(super) const NAME: &str = "fsevents";

/// How often the stream's run loop checks whether to stop.
const STOP_CHECK_SECONDS: f64 = 0.25;

/// The stream's thread; dropping it stops the stream.
pub(super) struct Handle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Handle {
    /// Nothing to do: the stream covers the whole tree whatever git ignores,
    /// and the coalescer drops what it ignores.
    #[expect(clippy::unused_self, reason = "same interface as inotify's")]
    pub(super) fn reevaluate(&self, _rules_changed: bool) {}
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.join().ok();
        }
    }
}

/// Starts the stream before returning; `Err` with a reason when `FSEvents`
/// refuses it.
pub(super) fn start(
    root: &Root,
    tx: Sender<Raw>,
    shared: &Arc<Shared>,
    _excludes: &SharedExcludes,
) -> Result<Handle, String> {
    let mut paths = vec![root.worktree.clone()];
    for dir in [&root.git_dir, &root.common_dir] {
        if !dir.starts_with(&root.worktree) && !paths.contains(dir) {
            paths.push(dir.clone());
        }
    }
    let count = paths.len();
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = Arc::clone(&stop);
    let (ready_tx, ready) = mpsc::channel();
    let thread = thread::Builder::new()
        .name("dffy-fsevents".into())
        .spawn(move || run(&paths, tx, &ready_tx, &stopped))
        .map_err(|e| e.to_string())?;
    match ready.recv() {
        Ok(Ok(())) => {
            shared.watches.store(count, Ordering::Relaxed);
            Ok(Handle {
                stop,
                thread: Some(thread),
            })
        }
        Ok(Err(reason)) => Err(reason),
        Err(_) => Err("the FSEvents thread exited".to_owned()),
    }
}

fn run(paths: &[PathBuf], tx: Sender<Raw>, ready: &Sender<Result<(), String>>, stop: &AtomicBool) {
    let strings: Vec<_> = paths
        .iter()
        .map(|p| CFString::from_str(&p.to_string_lossy()))
        .collect();
    let array = CFArray::from_retained_objects(&strings);
    let info = Box::into_raw(Box::new(tx));
    let mut context = FSEventStreamContext {
        version: 0,
        info: info.cast(),
        retain: None,
        release: None,
        copyDescription: None,
    };
    // SAFETY: `callback` matches FSEvents' callback signature, `context` is
    // copied by the call, and `info` outlives the stream: it's freed only
    // after the stream is released below.
    let stream = unsafe {
        FSEventStreamCreate(
            None,
            Some(callback),
            &raw mut context,
            array.as_opaque(),
            kFSEventStreamEventIdSinceNow,
            0.0,
            kFSEventStreamCreateFlagFileEvents | kFSEventStreamCreateFlagNoDefer,
        )
    };
    let free_info = || {
        // SAFETY: `info` came from `Box::into_raw` above, and the stream that
        // used it is gone or was never created.
        drop(unsafe { Box::from_raw(info) });
    };
    if stream.is_null() {
        free_info();
        ready
            .send(Err("FSEvents refused the stream".to_owned()))
            .ok();
        return;
    }
    // SAFETY: Core Foundation initializes this constant before any of our
    // code runs and never writes it again.
    let mode = unsafe { kCFRunLoopDefaultMode };
    let (Some(run_loop), Some(mode)) = (CFRunLoop::current(), mode) else {
        // SAFETY: `stream` is valid and was never scheduled or started.
        unsafe { FSEventStreamRelease(stream) };
        free_info();
        ready.send(Err("no run loop for FSEvents".to_owned())).ok();
        return;
    };
    #[expect(
        deprecated,
        reason = "the replacement schedules on a dispatch queue, which needs another dependency for the same behavior"
    )]
    // SAFETY: `stream` is valid and not yet scheduled. It's scheduled on
    // this thread's run loop, which runs below until the handle asks to
    // stop, and it's invalidated before `run_loop` goes out of scope.
    unsafe {
        FSEventStreamScheduleWithRunLoop(stream, &run_loop, mode);
    }
    // SAFETY: `stream` is valid and scheduled on a run loop, as starting
    // requires.
    let started = unsafe { FSEventStreamStart(stream) };
    if started {
        ready.send(Ok(())).ok();
        while !stop.load(Ordering::Relaxed) {
            CFRunLoop::run_in_mode(Some(mode), STOP_CHECK_SECONDS, false);
        }
        // SAFETY: `stream` was started on this thread.
        unsafe { FSEventStreamStop(stream) };
    } else {
        ready
            .send(Err("FSEvents couldn't start the stream".to_owned()))
            .ok();
    }
    // SAFETY: `stream` is valid and stopped, or was never started; once
    // invalidated, FSEvents no longer calls `callback` for it.
    unsafe { FSEventStreamInvalidate(stream) };
    // SAFETY: `stream` is invalidated and this is its only reference.
    unsafe { FSEventStreamRelease(stream) };
    free_info();
}

/// Forwards each event's path, or an overflow when `FSEvents` says events
/// were dropped or a subtree must be rescanned.
unsafe extern "C-unwind" fn callback(
    _stream: ConstFSEventStreamRef,
    info: *mut c_void,
    count: usize,
    paths: NonNull<c_void>,
    flags: NonNull<FSEventStreamEventFlags>,
    _ids: NonNull<FSEventStreamEventId>,
) {
    const LOST: u32 = kFSEventStreamEventFlagMustScanSubDirs
        | kFSEventStreamEventFlagUserDropped
        | kFSEventStreamEventFlagKernelDropped;
    // SAFETY: `info` is the sender boxed in `run`, freed only after the
    // stream is invalidated, so it's alive whenever this is called.
    let tx = unsafe { &*info.cast::<Sender<Raw>>() };
    // SAFETY: without `kFSEventStreamCreateFlagUseCFTypes`, `paths` is an
    // array of `count` C string pointers, valid until this returns.
    let paths = unsafe { slice::from_raw_parts(paths.cast::<*const c_char>().as_ptr(), count) };
    // SAFETY: `flags` has `count` entries, valid until this returns.
    let flags = unsafe { slice::from_raw_parts(flags.as_ptr(), count) };
    for (&path, &flag) in paths.iter().zip(flags) {
        // SAFETY: each entry is a NUL-terminated path FSEvents owns until
        // this returns, and it's copied before then.
        let path = unsafe { CStr::from_ptr(path) };
        let raw = if flag & LOST == 0 {
            Raw::Path {
                path: PathBuf::from(OsStr::from_bytes(path.to_bytes())),
                is_dir: flag & kFSEventStreamEventFlagItemIsDir != 0,
            }
        } else {
            Raw::Overflow
        };
        tx.send(raw).ok();
    }
}
