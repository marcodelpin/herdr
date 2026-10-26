//! The server's console stop handler (herdr-ir4j, ADR-0014 rule 1).
//!
//! A console control handler that returns TRUE suppresses the default one,
//! which ends the process. The handler is therefore registered only once the
//! thread that acts on its events runs: the thread first, the handler second.
//! When the thread cannot start no handler is registered, and a console event
//! still ends the process the default way. ctrlc 3.5.2 does it the other way
//! round and leaves its handler registered when its thread cannot start, so
//! every Ctrl-C/Ctrl-Break from then on is swallowed (codex ir4j-r3 finding 3).

use std::io;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
use windows_sys::Win32::System::Threading::{
    CreateSemaphoreW, ReleaseSemaphore, WaitForSingleObject, INFINITE,
};

use crate::platform::ServerQuitSignal;

/// Events counted but not yet dispatched; more are dropped (the stop is
/// already pending).
const MAX_PENDING_EVENTS: i32 = 255;

/// The semaphore the native handler releases, null while no dispatch thread
/// waits on it: the handler then declines the event and the default handler
/// ends the process.
static EVENTS: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(ptr::null_mut());
static INSTALLED: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn console_stop_handler(_event: u32) -> BOOL {
    let events = EVENTS.load(Ordering::Acquire);
    if events.is_null() {
        return 0;
    }
    // SAFETY: a semaphore handle this module created and never closes while
    // it is published.
    unsafe { ReleaseSemaphore(events, 1, ptr::null_mut()) };
    1
}

/// A semaphore handle moved into the dispatch thread.
struct Events(HANDLE);

// SAFETY: a kernel handle, usable from any thread.
unsafe impl Send for Events {}

/// Routes console control events to `on_quit` from now on, once per process.
/// If that is not possible the default handler stays the only one, and a
/// console event ends the process as it did before.
pub(crate) fn spawn_server_signal_monitor(
    on_quit: impl Fn(ServerQuitSignal) + Send + Sync + 'static,
) {
    if INSTALLED.swap(true, Ordering::AcqRel) {
        tracing::warn!("server stop handler already installed");
        return;
    }
    if let Err(err) = install(on_quit) {
        tracing::warn!(%err, "failed to install server stop handler; a console control event ends the server at once");
    }
}

fn install(on_quit: impl Fn(ServerQuitSignal) + Send + Sync + 'static) -> io::Result<()> {
    // SAFETY: plain object creation; checked below.
    let events = unsafe { CreateSemaphoreW(ptr::null(), 0, MAX_PENDING_EVENTS, ptr::null()) };
    if events.is_null() {
        return Err(io::Error::last_os_error());
    }
    let dispatched = Events(events);
    let spawned = crate::thread_spawn::spawn_named("herdr-console-stop", move || {
        dispatch(dispatched, on_quit);
    });
    if let Err(err) = spawned {
        // SAFETY: never published, and the thread that would own it never ran.
        unsafe { CloseHandle(events) };
        return Err(err);
    }
    EVENTS.store(events, Ordering::Release);
    // SAFETY: registers a handler with the 'static lifetime of this module.
    if unsafe { SetConsoleCtrlHandler(Some(console_stop_handler), 1) } == 0 {
        let err = io::Error::last_os_error();
        // The dispatch thread stays parked on a semaphore nothing releases.
        EVENTS.store(ptr::null_mut(), Ordering::Release);
        return Err(err);
    }
    Ok(())
}

fn dispatch(events: Events, on_quit: impl Fn(ServerQuitSignal)) {
    loop {
        // SAFETY: the semaphore stays open for the life of the process.
        let waited = unsafe { WaitForSingleObject(events.0, INFINITE) };
        if waited != WAIT_OBJECT_0 {
            // Nothing acts on an event any more: give the default back.
            EVENTS.store(ptr::null_mut(), Ordering::Release);
            tracing::error!(err = %io::Error::last_os_error(), "server stop handler dispatch failed; a console control event ends the server at once");
            return;
        }
        on_quit(ServerQuitSignal::ConsoleControl);
    }
}
