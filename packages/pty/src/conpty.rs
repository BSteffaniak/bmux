//! Windows pseudo-consoles (`ConPTY`): the process sees a real console, can be resized and typed
//! into, and runs in a Job Object so killing reaches everything it started.
//!
//! A console's `ctrl+c` cannot be raised from outside it (writing it as input does not, and
//! `GenerateConsoleCtrlEvent` would need this process to leave its own console), so interrupting
//! stops the process and what it started, as killing does; [`PtyCapabilities::signals`] says so.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Write as _};
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};

use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE, S_OK};
use windows_sys::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
};
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList,
    PROC_THREAD_ATTRIBUTE_JOB_LIST, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, UpdateProcThreadAttribute, WaitForSingleObject,
};

use super::{
    Events, Exit, PtyCapabilities, PtyCommand, PtyControl, PtyEvent, PtyExit, PtyProcess, PtySize,
    PtyWriter, QUEUE,
};

pub const CAPABILITIES: PtyCapabilities = PtyCapabilities {
    terminal: true,
    resize: true,
    input: true,
    signals: false,
};

/// Lets `ConPTY` reflow its buffer on resize without repainting it into the output.
const PSEUDOCONSOLE_RESIZE_QUIRK: u32 = 0x2;

/// The pseudo-console, closed once the process exits so its output ends.
struct Console(Mutex<Option<HPCON>>);

impl Console {
    fn resize(&self, size: PtySize) -> io::Result<()> {
        let console = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(console) = *console else {
            return Ok(());
        };
        // SAFETY: an open pseudo-console handle, guarded against a concurrent close.
        let result = unsafe { ResizePseudoConsole(console, coord(size)) };
        if result == S_OK {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(result))
        }
    }

    fn close(&self) {
        if let Some(console) = self.0.lock().unwrap_or_else(PoisonError::into_inner).take() {
            // SAFETY: closed exactly once; taken out so nothing uses it afterwards.
            unsafe { ClosePseudoConsole(console) };
        }
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        self.close();
    }
}

impl std::fmt::Debug for Console {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Console")
    }
}

#[derive(Debug)]
pub struct Control {
    pid: u32,
    job: OwnedHandle,
    console: Arc<Console>,
    /// The console's input, as typing reaches it.
    input: Mutex<File>,
    closed: Arc<AtomicBool>,
    writer: Mutex<Option<PtyWriter>>,
}

impl Control {
    pub const fn id(&self) -> u32 {
        self.pid
    }

    // Uniform with the other backends, whose capabilities are fixed too.
    #[allow(clippy::unused_self)]
    pub const fn capabilities(&self) -> PtyCapabilities {
        CAPABILITIES
    }

    pub fn resize(&self, size: PtySize) -> io::Result<()> {
        self.console.resize(size)
    }

    /// Stop the process and what it started (see the module documentation).
    pub fn interrupt(&self) -> io::Result<()> {
        self.kill()
    }

    /// Terminate the Job Object: the process and everything it started, even after it exited.
    pub fn kill(&self) -> io::Result<()> {
        // SAFETY: an owned job handle.
        if unsafe { TerminateJobObject(self.job.as_raw_handle(), 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn stop_output(&self) {
        self.closed.store(true, Ordering::Release);
    }

    pub fn writer(&self) -> io::Result<PtyWriter> {
        let mut writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(writer) = writer.as_ref() {
            return Ok(writer.clone());
        }
        let mut input = self
            .input
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .try_clone()?;
        let (sender, queued) = mpsc::sync_channel::<Vec<u8>>(QUEUE);
        std::thread::spawn(move || {
            for bytes in queued {
                if input.write_all(&bytes).is_err() {
                    break;
                }
            }
        });
        let created = PtyWriter { sender };
        *writer = Some(created.clone());
        drop(writer);
        Ok(created)
    }
}

fn coord(size: PtySize) -> COORD {
    COORD {
        X: i16::try_from(size.cols).unwrap_or(i16::MAX),
        Y: i16::try_from(size.rows).unwrap_or(i16::MAX),
    }
}

/// A pipe's two ends.
fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let mut read: HANDLE = null_mut();
    let mut write: HANDLE = null_mut();
    // SAFETY: valid out-pointers; default security, so the ends are not inheritable.
    if unsafe { CreatePipe(&raw mut read, &raw mut write, null(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreatePipe returned two new handles that nothing else owns.
    Ok(unsafe {
        (
            OwnedHandle::from_raw_handle(read),
            OwnedHandle::from_raw_handle(write),
        )
    })
}

/// A Job Object that kills what it holds when the last handle closes.
fn job() -> io::Result<OwnedHandle> {
    // SAFETY: an unnamed job with default security.
    let job = unsafe { CreateJobObjectW(null(), null()) };
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateJobObjectW returned a new handle that nothing else owns.
    let job = unsafe { OwnedHandle::from_raw_handle(job) };
    // SAFETY: zeroed is a valid empty limit structure.
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: the structure outlives the call and its size is passed.
    if unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            u32::try_from(size_of_val(&limits)).unwrap_or(u32::MAX),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(job)
}

fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(std::iter::once(0)).collect()
}

/// `arg` quoted as the C runtime splits command lines.
fn quote(arg: &OsStr, line: &mut Vec<u16>) {
    let units: Vec<u16> = arg.encode_wide().collect();
    let needs_quotes = units.is_empty()
        || units
            .iter()
            .any(|unit| matches!(*unit, 0x20 | 0x09 | 0x0a | 0x0b | 0x22));
    if !needs_quotes {
        line.extend(units);
        return;
    }
    line.push(u16::from(b'"'));
    let mut backslashes = 0;
    for unit in units {
        if unit == u16::from(b'\\') {
            backslashes += 1;
            continue;
        }
        let escaped = if unit == u16::from(b'"') {
            backslashes * 2 + 1
        } else {
            backslashes
        };
        line.extend(std::iter::repeat_n(u16::from(b'\\'), escaped));
        line.push(unit);
        backslashes = 0;
    }
    line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2));
    line.push(u16::from(b'"'));
}

fn command_line(spec: &PtyCommand) -> Vec<u16> {
    let mut line = Vec::new();
    quote(&spec.program, &mut line);
    for arg in &spec.args {
        line.push(u16::from(b' '));
        quote(arg, &mut line);
    }
    line.push(0);
    line
}

/// The environment block: inherited, then changed as `spec` says. Names are compared without
/// case, as Windows does.
fn environment(spec: &PtyCommand) -> Vec<u16> {
    let key = |name: &OsStr| name.to_string_lossy().to_uppercase();
    let mut variables: BTreeMap<String, (OsString, OsString)> = std::env::vars_os()
        .map(|(name, value)| (key(&name), (name, value)))
        .collect();
    if !spec.env.iter().any(|(name, _)| key(name) == "TERM") {
        variables.insert("TERM".to_owned(), ("TERM".into(), "xterm-256color".into()));
    }
    for (name, value) in &spec.env {
        match value {
            Some(value) => {
                variables.insert(key(name), (name.clone(), value.clone()));
            }
            None => {
                variables.remove(&key(name));
            }
        }
    }
    let mut block = Vec::new();
    for (name, value) in variables.values() {
        block.extend(name.encode_wide());
        block.push(u16::from(b'='));
        block.extend(value.encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}

/// Start the process on a new pseudo-console of `size`, inside `job`, without running it before
/// it belongs to the job.
fn start(spec: &PtyCommand, console: HPCON, job: &OwnedHandle) -> io::Result<(OwnedHandle, u32)> {
    let mut command_line = command_line(spec);
    let environment = environment(spec);
    let cwd = spec.cwd.as_ref().map(|cwd| wide(cwd.as_os_str()));
    let mut size = 0;
    // SAFETY: a size query with no list.
    unsafe { InitializeProcThreadAttributeList(null_mut(), 2, 0, &raw mut size) };
    let mut storage = vec![0usize; size.div_ceil(size_of::<usize>())];
    let attributes = storage.as_mut_ptr().cast();
    // SAFETY: storage of the queried size.
    if unsafe { InitializeProcThreadAttributeList(attributes, 2, 0, &raw mut size) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let jobs = [job.as_raw_handle()];
    let started = (|| {
        // SAFETY: the attribute values outlive CreateProcessW below.
        if unsafe {
            UpdateProcThreadAttribute(
                attributes,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                console as *const core::ffi::c_void,
                size_of::<HPCON>(),
                null_mut(),
                null(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // Assigned to the job as it is created, so nothing it starts can escape.
        // SAFETY: as above.
        if unsafe {
            UpdateProcThreadAttribute(
                attributes,
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                jobs.as_ptr().cast(),
                size_of_val(&jobs),
                null_mut(),
                null(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: zeroed is a valid empty startup structure.
        let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        startup.StartupInfo.cb = u32::try_from(size_of_val(&startup)).unwrap_or(u32::MAX);
        // Without explicit (invalid) standard handles, a process may inherit this one's
        // redirected handles instead of using the console.
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
        startup.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
        startup.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
        startup.lpAttributeList = attributes;
        // SAFETY: zeroed is a valid empty process structure.
        let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: every pointer is valid for the call; strings are terminated.
        if unsafe {
            CreateProcessW(
                null(),
                command_line.as_mut_ptr(),
                null(),
                null(),
                0,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                environment.as_ptr().cast(),
                cwd.as_ref().map_or(null(), Vec::as_ptr),
                &raw const startup.StartupInfo,
                &raw mut process,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateProcessW returned new handles that nothing else owns.
        let (process_handle, thread) = unsafe {
            (
                OwnedHandle::from_raw_handle(process.hProcess),
                OwnedHandle::from_raw_handle(process.hThread),
            )
        };
        drop(thread);
        Ok((process_handle, process.dwProcessId))
    })();
    // SAFETY: initialized above and no longer used.
    unsafe { DeleteProcThreadAttributeList(attributes) };
    started
}

pub fn spawn(spec: &PtyCommand, size: PtySize) -> io::Result<PtyProcess> {
    let (console_input, input) = pipe()?;
    let (output, console_output) = pipe()?;
    let mut console: HPCON = 0;
    // SAFETY: valid handles and out-pointer; ConPTY duplicates the pipe ends it is given.
    let result = unsafe {
        CreatePseudoConsole(
            coord(size),
            console_input.as_raw_handle(),
            console_output.as_raw_handle(),
            PSEUDOCONSOLE_RESIZE_QUIRK,
            &raw mut console,
        )
    };
    if result != S_OK {
        return Err(io::Error::from_raw_os_error(result));
    }
    // Only the console holds these ends now, so its closing ends the output.
    drop((console_input, console_output));
    let console = Arc::new(Console(Mutex::new(Some(console))));
    let job = job()?;
    let raw_console = console
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .unwrap_or(0);
    let (process, pid) = start(spec, raw_console, &job)?;
    let (events, receiver) = Events::new();
    let (done, finished) = mpsc::channel();
    events.read(File::from(output), PtyEvent::Output, done);
    let exited = Arc::new(AtomicBool::new(false));
    let closed = Arc::clone(&events.closed);
    let control = PtyControl::new(
        super::platform::Control::Conpty(Control {
            pid,
            job,
            console: Arc::clone(&console),
            input: Mutex::new(File::from(input)),
            closed,
            writer: Mutex::new(None),
        }),
        size,
    );
    super::report_exit(
        events,
        Exit {
            readers: 1,
            finished,
            exited,
            grace: spec.drain_grace,
            status: Arc::clone(&control.status),
        },
        move || {
            // SAFETY: an owned process handle.
            unsafe { WaitForSingleObject(process.as_raw_handle(), INFINITE) };
            let mut code = 0u32;
            // SAFETY: an owned process handle and a valid out-pointer.
            let known = unsafe { GetExitCodeProcess(process.as_raw_handle(), &raw mut code) } != 0;
            // The console's output only ends once it is closed.
            console.close();
            known.then(|| PtyExit {
                code: Some(i32::from_ne_bytes(code.to_ne_bytes())),
                signal: None,
                complete: true,
            })
        },
    );
    Ok(PtyProcess {
        events: receiver,
        control,
        exited: false,
    })
}
