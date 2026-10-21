//! herdr-ir4j r4, codex ir4j-r3 finding 3: when the server's console stop
//! handler cannot start the thread that acts on its events, a console control
//! event must still end the process by the default handler (ADR-0014 rule 1),
//! not be swallowed by a handler nobody dispatches.
//!
//! The thread spawn is refused for real, not through a hook of the code under
//! test: the test re-runs itself in a child with its own console, which puts
//! itself into a job whose commit limit is its current commit, so the OS
//! refuses the stack of any new thread. The limit is lifted again before the
//! child sends itself a Ctrl-Break.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Console::{
    GenerateConsoleCtrlEvent, SetConsoleCtrlHandler, CTRL_BREAK_EVENT,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, CREATE_NEW_CONSOLE};

const CHILD_ENV: &str = "HERDR_TEST_CONSOLE_STOP_CHILD";
/// The exit status of a process ended by the default console handler.
const STATUS_CONTROL_C_EXIT: u32 = 0xC000_013A;
const SURVIVED_EXIT: i32 = 3;
const INJECTED: &str = "probe thread spawn under the commit limit failed";
const DISPATCHED: &str = "console stop event dispatched";

#[repr(C)]
#[derive(Default)]
struct ProcessMemoryCountersEx {
    cb: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
    private_usage: usize,
}

#[link(name = "kernel32")]
extern "system" {
    fn K32GetProcessMemoryInfo(
        process: HANDLE,
        counters: *mut ProcessMemoryCountersEx,
        cb: u32,
    ) -> BOOL;
}

unsafe extern "system" fn warm_up_handler(_event: u32) -> BOOL {
    0
}

fn private_usage() -> usize {
    let mut counters = ProcessMemoryCountersEx {
        cb: std::mem::size_of::<ProcessMemoryCountersEx>() as u32,
        ..Default::default()
    };
    // SAFETY: a correctly sized out structure for the current process.
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    assert_ne!(ok, 0, "{}", std::io::Error::last_os_error());
    counters.private_usage
}

fn set_commit_limit(job: HANDLE, limit: Option<usize>) {
    // SAFETY: plain data, zero is a valid "no limit" value.
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    if let Some(limit) = limit {
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY;
        info.ProcessMemoryLimit = limit;
    }
    // SAFETY: a job handle this process created and a matching structure.
    let ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    assert_ne!(ok, 0, "{}", std::io::Error::last_os_error());
}

/// Runs in the child: installs the server stop handler while no thread can
/// start, then sends itself a Ctrl-Break. Ends by the default handler, or
/// with `SURVIVED_EXIT` when the event was swallowed.
fn child_installs_the_handler_without_threads_then_breaks() -> ! {
    // Allocations the steps under the limit make, made once before it, so
    // only the new thread's stack can hit the limit.
    std::thread::Builder::new()
        .name("ctrl-c".into())
        .spawn(|| {})
        .expect("warm-up thread")
        .join()
        .expect("warm-up thread join");
    // SAFETY: registers and removes a handler that declines every event.
    unsafe {
        SetConsoleCtrlHandler(Some(warm_up_handler), 1);
        SetConsoleCtrlHandler(Some(warm_up_handler), 0);
    }
    // SAFETY: an unnamed job for this process only.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    assert!(!job.is_null(), "{}", std::io::Error::last_os_error());
    // SAFETY: the pseudo handle of the current process.
    let assigned = unsafe { AssignProcessToJobObject(job, GetCurrentProcess()) };
    assert_ne!(assigned, 0, "{}", std::io::Error::last_os_error());

    set_commit_limit(job, Some(private_usage()));
    let probe = std::thread::Builder::new()
        .name("ctrl-c".into())
        .spawn(|| {});
    crate::platform::spawn_server_signal_monitor(|_| eprintln!("{DISPATCHED}"));
    set_commit_limit(job, None);

    match probe {
        Ok(handle) => {
            let _ = handle.join();
            eprintln!("probe thread spawn under the commit limit succeeded");
        }
        Err(err) => eprintln!("{INJECTED}: {err}"),
    }
    // SAFETY: CTRL_BREAK_EVENT to every process of this console, which is
    // this process alone.
    let sent = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, 0) };
    assert_ne!(sent, 0, "{}", std::io::Error::last_os_error());
    std::thread::sleep(Duration::from_secs(10));
    eprintln!("survived the Ctrl-Break");
    std::process::exit(SURVIVED_EXIT);
}

#[test]
fn console_break_ends_the_server_when_its_stop_handler_thread_cannot_start() {
    if std::env::var_os(CHILD_ENV).is_some() {
        child_installs_the_handler_without_threads_then_breaks();
    }
    let module = module_path!();
    let module = module.split_once("::").map_or(module, |(_, rest)| rest);
    let name = format!(
        "{module}::console_break_ends_the_server_when_its_stop_handler_thread_cannot_start"
    );

    use std::os::windows::process::CommandExt;
    let mut child = Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", &name, "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, "1")
        .creation_flags(CREATE_NEW_CONSOLE)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the child test process");
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait for the child") {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let mut output = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        let _ = stderr.read_to_string(&mut output);
    }
    assert!(
        output.contains(INJECTED),
        "the commit limit did not refuse a thread, the test proves nothing ({status:?}):\n{output}"
    );
    assert!(
        !output.contains(DISPATCHED),
        "the stop handler's thread started under the commit limit, the test proves nothing:\n{output}"
    );
    let code = status
        .and_then(|status| status.code())
        .map(|code| code as u32);
    assert_eq!(
        code,
        Some(STATUS_CONTROL_C_EXIT),
        "Ctrl-Break was swallowed: the process did not end by the default handler \
         after the stop handler's thread failed to start ({status:?}):\n{output}"
    );
}
