//! Pseudo-consoles (`ConPTY`) as terminal pairs.
//!
//! The console asks the terminal for the cursor position (`CSI 6 n`) when it starts, so it can
//! take over a cursor already placed, and it blocks the first process until that is answered:
//! the reader of the master must answer (`CSI row ; col R`). It also asks for win32 input mode,
//! which a terminal may ignore.
//!
//! A `conpty.dll` shipped beside the executable (with its `OpenConsole.exe`) is used in place
//! of the system's console host, as newer releases fix many console bugs. Only the executable's
//! own directory is searched for it.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle, RawHandle};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, HANDLE, INVALID_HANDLE_VALUE, S_OK,
    WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
};
use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_APPLICATION_DIR, LoadLibraryExW,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, REG_EXPAND_SZ, REG_SZ, RegCloseKey,
    RegEnumValueW, RegOpenKeyExW, RegQueryInfoKeyW,
};
use windows_sys::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};
use windows_sys::core::HRESULT;

use super::{ChildExit, SpawnCommand};
use crate::PtySize;
use crate::conpty::{pipe, quote, wide};

/// Keep the cursor the terminal already has: the console asks for it on start.
const PSEUDOCONSOLE_INHERIT_CURSOR: u32 = 0x1;
/// Let the console reflow its buffer on resize without repainting it into the output.
const PSEUDOCONSOLE_RESIZE_QUIRK: u32 = 0x2;
/// Ask the terminal for win32 input mode, which carries every key event.
const PSEUDOCONSOLE_WIN32_INPUT_MODE: u32 = 0x4;

/// The exit code of a process ended by [`Child::kill`].
const KILLED_EXIT_CODE: u32 = 1;

type Create = unsafe extern "system" fn(COORD, HANDLE, HANDLE, u32, *mut HPCON) -> HRESULT;
type Resize = unsafe extern "system" fn(HPCON, COORD) -> HRESULT;
type Close = unsafe extern "system" fn(HPCON);

/// The pseudo-console functions in use: a sideloaded `conpty.dll`'s, or the system's.
struct Functions {
    create: Create,
    resize: Resize,
    close: Close,
}

fn functions() -> &'static Functions {
    static FUNCTIONS: OnceLock<Functions> = OnceLock::new();
    FUNCTIONS.get_or_init(|| {
        sideloaded().unwrap_or(Functions {
            create: CreatePseudoConsole,
            resize: ResizePseudoConsole,
            close: ClosePseudoConsole,
        })
    })
}

/// The functions of a `conpty.dll` in the executable's directory, which stays loaded.
fn sideloaded() -> Option<Functions> {
    let name = wide(OsStr::new("conpty.dll"));
    // SAFETY: a terminated name; only the application directory is searched.
    let library = unsafe {
        LoadLibraryExW(
            name.as_ptr(),
            null_mut(),
            LOAD_LIBRARY_SEARCH_APPLICATION_DIR,
        )
    };
    if library.is_null() {
        return None;
    }
    // SAFETY: a loaded library and terminated names.
    let (create, resize, close) = unsafe {
        (
            GetProcAddress(library, c"CreatePseudoConsole".as_ptr().cast())?,
            GetProcAddress(library, c"ResizePseudoConsole".as_ptr().cast())?,
            GetProcAddress(library, c"ClosePseudoConsole".as_ptr().cast())?,
        )
    };
    // SAFETY: these exports have the documented pseudo-console signatures.
    unsafe {
        Some(Functions {
            create: std::mem::transmute::<unsafe extern "system" fn() -> isize, Create>(create),
            resize: std::mem::transmute::<unsafe extern "system" fn() -> isize, Resize>(resize),
            close: std::mem::transmute::<unsafe extern "system" fn() -> isize, Close>(close),
        })
    }
}

/// `result` as an error: a Win32 error code where it wraps one.
fn hresult_error(result: HRESULT, doing: &str) -> io::Error {
    let bits = u32::from_ne_bytes(result.to_ne_bytes());
    // FACILITY_WIN32 failures carry a Win32 error code in their low word.
    if bits & 0xFFFF_0000 == 0x8007_0000 {
        let error =
            io::Error::from_raw_os_error(i32::from(u16::try_from(bits & 0xFFFF).unwrap_or(0)));
        return io::Error::new(error.kind(), format!("{doing}: {error}"));
    }
    io::Error::other(format!("{doing}: HRESULT {bits:#010x}"))
}

fn coord(size: PtySize) -> COORD {
    COORD {
        X: i16::try_from(size.cols).unwrap_or(i16::MAX),
        Y: i16::try_from(size.rows).unwrap_or(i16::MAX),
    }
}

/// An open pseudo-console, closed when dropped.
struct Console(HPCON);

impl Drop for Console {
    fn drop(&mut self) {
        // SAFETY: an open console, closed exactly once.
        unsafe { (functions().close)(self.0) };
    }
}

/// What the master and slave share: the console, closed with the last of them.
struct Shared {
    console: Console,
    size: Mutex<PtySize>,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Console")
            .field(
                "size",
                &*self.size.lock().unwrap_or_else(PoisonError::into_inner),
            )
            .finish_non_exhaustive()
    }
}

pub fn open(size: PtySize) -> io::Result<(Master, Slave)> {
    let (console_input, input) = pipe()?;
    let (output, console_output) = pipe()?;
    let mut console: HPCON = 0;
    // SAFETY: valid handles and out-pointer; the console duplicates the pipe ends it is given.
    let result = unsafe {
        (functions().create)(
            coord(size),
            console_input.as_raw_handle(),
            console_output.as_raw_handle(),
            PSEUDOCONSOLE_INHERIT_CURSOR
                | PSEUDOCONSOLE_RESIZE_QUIRK
                | PSEUDOCONSOLE_WIN32_INPUT_MODE,
            &raw mut console,
        )
    };
    if result != S_OK {
        return Err(hresult_error(result, "cannot create a pseudo-console"));
    }
    // Only the console holds these ends now, so its closing ends the output.
    drop((console_input, console_output));
    let shared = Arc::new(Shared {
        console: Console(console),
        size: Mutex::new(size),
    });
    Ok((
        Master {
            shared: Arc::clone(&shared),
            output: File::from(output),
            input: Mutex::new(Some(File::from(input))),
        },
        Slave { shared },
    ))
}

#[derive(Debug)]
pub struct Master {
    shared: Arc<Shared>,
    output: File,
    input: Mutex<Option<File>>,
}

impl Master {
    pub fn resize(&self, size: PtySize) -> io::Result<()> {
        let mut current = self
            .shared
            .size
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // SAFETY: an open console; the size lock orders resizes.
        let result = unsafe { (functions().resize)(self.shared.console.0, coord(size)) };
        if result != S_OK {
            return Err(hresult_error(result, "cannot resize the pseudo-console"));
        }
        *current = size;
        Ok(())
    }

    // Fallible on every platform: Unix asks the terminal.
    #[allow(clippy::unnecessary_wraps)]
    pub fn size(&self) -> io::Result<PtySize> {
        Ok(*self
            .shared
            .size
            .lock()
            .unwrap_or_else(PoisonError::into_inner))
    }

    pub fn try_clone_reader(&self) -> io::Result<Reader> {
        Ok(Reader(self.output.try_clone()?))
    }

    pub fn take_writer(&self) -> io::Result<Input> {
        self.input
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .map(Input)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "the terminal's writer was already taken",
                )
            })
    }
}

#[derive(Debug)]
pub struct Reader(File);

impl Reader {
    pub fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self.0.read(buffer) {
            // The console closed its end.
            Err(error)
                if error.raw_os_error()
                    == Some(i32::try_from(ERROR_BROKEN_PIPE).unwrap_or(i32::MAX)) =>
            {
                Ok(0)
            }
            other => other,
        }
    }
}

#[derive(Debug)]
pub struct Input(File);

impl Input {
    pub fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes)
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

#[derive(Debug)]
pub struct Slave {
    shared: Arc<Shared>,
}

impl Slave {
    pub fn spawn(&self, spec: &SpawnCommand) -> io::Result<Child> {
        let argv = spec.argv();
        let program = argv
            .first()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no program to run"))?;
        let resolved = search_path(program, spec.get_env("PATH"), spec.get_env("PATHEXT"));
        let mut line = Vec::new();
        quote(resolved.as_deref().unwrap_or(program), &mut line);
        for arg in &argv[1..] {
            if arg.encode_wide().any(|unit| unit == 0) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("argument {} contains a nul character", arg.display()),
                ));
            }
            line.push(u16::from(b' '));
            quote(arg, &mut line);
        }
        line.push(0);
        // Without a resolved path, CreateProcessW looks the program up itself.
        let application = resolved.as_deref().map(wide);
        let environment = environment_block(spec);
        let cwd = working_directory(spec).map(|dir| wide(dir.as_os_str()));
        start(
            &mut line,
            application.as_deref(),
            &environment,
            cwd.as_deref(),
            self.shared.console.0,
        )
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "cannot start {} in {}: {error}",
                    String::from_utf16_lossy(&line[..line.len() - 1]),
                    cwd.as_deref().map_or_else(
                        || "the current directory".to_owned(),
                        |cwd| String::from_utf16_lossy(&cwd[..cwd.len() - 1]),
                    ),
                ),
            )
        })
    }
}

/// `program` found on `path`, as given or with each `PATHEXT` extension, when it is a file.
fn search_path(
    program: &OsStr,
    path: Option<&OsStr>,
    extensions: Option<&OsStr>,
) -> Option<OsString> {
    let path = path?;
    let extensions = extensions.unwrap_or(OsStr::new(".EXE"));
    for directory in std::env::split_paths(path) {
        let candidate = directory.join(program);
        if candidate.is_file() {
            return Some(candidate.into_os_string());
        }
        for extension in std::env::split_paths(extensions) {
            let extension = extension.as_os_str().to_string_lossy();
            let extension = extension.strip_prefix('.').unwrap_or(&extension);
            let candidate = candidate.with_extension(extension);
            if candidate.is_file() {
                return Some(candidate.into_os_string());
            }
        }
    }
    None
}

/// The command's directory when it is one, otherwise `USERPROFILE` when that is one, made
/// absolute; `None` keeps this process's.
fn working_directory(spec: &SpawnCommand) -> Option<PathBuf> {
    let directory = spec
        .get_cwd()
        .filter(|dir| Path::new(dir).is_dir())
        .or_else(|| {
            spec.get_env("USERPROFILE")
                .filter(|dir| Path::new(dir).is_dir())
        })?;
    let directory = Path::new(directory);
    if directory.is_relative()
        && let Ok(current) = std::env::current_dir()
    {
        return Some(current.join(directory));
    }
    Some(directory.to_owned())
}

fn environment_block(spec: &SpawnCommand) -> Vec<u16> {
    let mut block = Vec::new();
    for (name, value) in spec.environment() {
        block.extend(name.encode_wide());
        block.push(u16::from(b'='));
        block.extend(value.encode_wide());
        block.push(0);
    }
    // An empty block still needs both terminators.
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

/// Start a process attached to `console`.
fn start(
    line: &mut [u16],
    application: Option<&[u16]>,
    environment: &[u16],
    cwd: Option<&[u16]>,
    console: HPCON,
) -> io::Result<Child> {
    let mut size = 0;
    // SAFETY: a size query with no list.
    unsafe { InitializeProcThreadAttributeList(null_mut(), 1, 0, &raw mut size) };
    let mut storage = vec![0usize; size.div_ceil(size_of::<usize>())];
    let attributes = storage.as_mut_ptr().cast();
    // SAFETY: storage of the queried size.
    if unsafe { InitializeProcThreadAttributeList(attributes, 1, 0, &raw mut size) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let started = (|| {
        // SAFETY: the console outlives CreateProcessW below.
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
        // SAFETY: every pointer is valid for the call; strings and the block are terminated.
        if unsafe {
            CreateProcessW(
                application.map_or(null(), <[u16]>::as_ptr),
                line.as_mut_ptr(),
                null(),
                null(),
                0,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                environment.as_ptr().cast(),
                cwd.map_or(null(), <[u16]>::as_ptr),
                &raw const startup.StartupInfo,
                &raw mut process,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateProcessW returned new handles that nothing else owns.
        let (handle, thread) = unsafe {
            (
                OwnedHandle::from_raw_handle(process.hProcess),
                OwnedHandle::from_raw_handle(process.hThread),
            )
        };
        drop(thread);
        Ok(Child {
            process: Arc::new(handle),
            pid: process.dwProcessId,
        })
    })();
    // SAFETY: initialized above and no longer used.
    unsafe { DeleteProcThreadAttributeList(attributes) };
    started
}

#[derive(Debug)]
pub struct Child {
    process: Arc<OwnedHandle>,
    pid: u32,
}

impl Child {
    pub const fn id(&self) -> u32 {
        self.pid
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ChildExit>> {
        if !exited(&self.process, 0)? {
            return Ok(None);
        }
        exit_code(&self.process).map(|code| Some(ChildExit::with_exit_code(code)))
    }

    pub fn wait(&mut self) -> io::Result<ChildExit> {
        exited(&self.process, INFINITE)?;
        exit_code(&self.process).map(ChildExit::with_exit_code)
    }

    pub fn kill(&mut self) -> io::Result<()> {
        terminate(&self.process)
    }

    pub fn killer(&self) -> io::Result<Killer> {
        Ok(Killer {
            process: Arc::new(self.process.try_clone()?),
        })
    }

    pub fn as_raw_handle(&self) -> RawHandle {
        self.process.as_raw_handle()
    }
}

#[derive(Debug, Clone)]
pub struct Killer {
    process: Arc<OwnedHandle>,
}

impl Killer {
    pub fn kill(&self) -> io::Result<()> {
        terminate(&self.process)
    }
}

/// Whether `process` exits within `timeout` milliseconds.
fn exited(process: &OwnedHandle, timeout: u32) -> io::Result<bool> {
    // SAFETY: an owned process handle.
    match unsafe { WaitForSingleObject(process.as_raw_handle(), timeout) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        WAIT_FAILED => Err(io::Error::last_os_error()),
        other => Err(io::Error::other(format!(
            "unexpected wait result {other:#x}"
        ))),
    }
}

fn exit_code(process: &OwnedHandle) -> io::Result<u32> {
    let mut code = 0u32;
    // SAFETY: an owned process handle and a valid out-pointer.
    if unsafe { GetExitCodeProcess(process.as_raw_handle(), &raw mut code) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(code)
}

/// Terminate `process`; one that has exited already counts as done.
fn terminate(process: &OwnedHandle) -> io::Result<()> {
    if exited(process, 0)? {
        return Ok(());
    }
    // SAFETY: an owned process handle.
    if unsafe { TerminateProcess(process.as_raw_handle(), KILLED_EXIT_CODE) } == 0 {
        let error = io::Error::last_os_error();
        // A process exiting meanwhile refuses termination.
        if exited(process, 0)? {
            return Ok(());
        }
        return Err(error);
    }
    Ok(())
}

/// Expand `%NAME%` references in a registry value against this process's environment.
fn expand(value: &[u16]) -> Option<OsString> {
    let mut source = value.to_vec();
    source.push(0);
    // SAFETY: a size query for a terminated source.
    let needed = unsafe { ExpandEnvironmentStringsW(source.as_ptr(), null_mut(), 0) };
    if needed == 0 {
        return None;
    }
    let mut expanded = vec![0u16; usize::try_from(needed).ok()?];
    // SAFETY: `expanded` is valid for `needed` units.
    let written =
        unsafe { ExpandEnvironmentStringsW(source.as_ptr(), expanded.as_mut_ptr(), needed) };
    if written == 0 || written > needed {
        return None;
    }
    let end = expanded
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(expanded.len());
    Some(OsString::from_wide(&expanded[..end]))
}

/// The string values of registry key `subkey` under `root`, expanded.
fn registry_strings(root: HKEY, subkey: &str) -> Vec<(OsString, OsString)> {
    let subkey = wide(OsStr::new(subkey));
    let mut key: HKEY = null_mut();
    // SAFETY: a predefined root, a terminated name and a valid out-pointer.
    if unsafe { RegOpenKeyExW(root, subkey.as_ptr(), 0, KEY_READ, &raw mut key) } != 0 {
        return Vec::new();
    }
    let mut longest_name = 0u32;
    let mut longest_value = 0u32;
    // SAFETY: an open key; only the lengths are asked for.
    let queried = unsafe {
        RegQueryInfoKeyW(
            key,
            null_mut(),
            null_mut(),
            null(),
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
            &raw mut longest_name,
            &raw mut longest_value,
            null_mut(),
            null_mut(),
        )
    };
    let mut values = Vec::new();
    if queried == 0 {
        let mut name = vec![0u16; usize::try_from(longest_name).unwrap_or(0) + 1];
        let mut data = vec![0u8; usize::try_from(longest_value).unwrap_or(0) + 2];
        let mut index = 0;
        loop {
            let mut name_length = u32::try_from(name.len()).unwrap_or(u32::MAX);
            let mut data_length = u32::try_from(data.len()).unwrap_or(u32::MAX);
            let mut kind = 0u32;
            // SAFETY: buffers valid for the lengths passed.
            let result = unsafe {
                RegEnumValueW(
                    key,
                    index,
                    name.as_mut_ptr(),
                    &raw mut name_length,
                    null(),
                    &raw mut kind,
                    data.as_mut_ptr(),
                    &raw mut data_length,
                )
            };
            if result == ERROR_MORE_DATA {
                // A value grew since the key was queried.
                name.resize(name.len() * 2, 0);
                data.resize(data.len() * 2, 0);
                continue;
            }
            if result == ERROR_NO_MORE_ITEMS || result != 0 {
                break;
            }
            index += 1;
            if kind != REG_SZ && kind != REG_EXPAND_SZ {
                continue;
            }
            let units: Vec<u16> = data[..usize::try_from(data_length).unwrap_or(0)]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes(*pair))
                .collect();
            let end = units
                .iter()
                .position(|unit| *unit == 0)
                .unwrap_or(units.len());
            let value = if kind == REG_EXPAND_SZ {
                match expand(&units[..end]) {
                    Some(value) => value,
                    None => continue,
                }
            } else {
                OsString::from_wide(&units[..end])
            };
            let name = OsString::from_wide(&name[..usize::try_from(name_length).unwrap_or(0)]);
            values.push((name, value));
        }
    }
    // SAFETY: opened above.
    unsafe { RegCloseKey(key) };
    values
}

/// This process's environment with the system's, then the user's, registered environment
/// applied over it; the user's `PATH` is appended to the system's.
pub fn base_environment() -> Vec<(OsString, OsString)> {
    let key = |name: &OsStr| name.to_string_lossy().to_lowercase();
    let mut environment: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    let set = |environment: &mut Vec<(OsString, OsString)>, name: OsString, value: OsString| {
        let wanted = key(&name);
        environment.retain(|(existing, _)| key(existing) != wanted);
        environment.push((name, value));
    };
    for (name, value) in registry_strings(
        HKEY_LOCAL_MACHINE,
        "System\\CurrentControlSet\\Control\\Session Manager\\Environment",
    ) {
        // The system key's USERNAME names the machine account, not the user.
        if key(&name) == "username" {
            continue;
        }
        set(&mut environment, name, value);
    }
    for (name, value) in registry_strings(HKEY_CURRENT_USER, "Environment") {
        let value = if key(&name) == "path" {
            match environment
                .iter()
                .find(|(existing, _)| key(existing) == "path")
            {
                Some((_, system)) => {
                    let mut joined = system.clone();
                    joined.push(";");
                    joined.push(&value);
                    joined
                }
                None => value,
            }
        } else {
            value
        };
        set(&mut environment, name, value);
    }
    environment
}
