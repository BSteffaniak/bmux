//! Suspended target creation inside a kill-on-close Job Object. Never signal by PID.
use super::windows_security::{Handle, wide};
use crate::protocol::{Operation, Response};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, WAIT_OBJECT_0};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicAccountingInformation,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, InitializeProcThreadAttributeList,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION,
    ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, UpdateProcThreadAttribute,
    WaitForSingleObject,
};

pub(super) struct Scope {
    job: Handle,
    target: Option<Handle>,
    pub(super) status: Response,
    swept: bool,
}
impl Scope {
    pub(super) fn new(token: String) -> io::Result<Self> {
        // SAFETY: unnamed, non-inheritable job owned only by this helper.
        let job = Handle::new(unsafe { CreateJobObjectW(null(), null()) })?;
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast(),
                size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            job,
            target: None,
            swept: false,
            status: Response {
                version: crate::protocol::VERSION,
                execution_token: token,
                started: false,
                target_pid: None,
                exit_code: None,
                exit_signal: None,
                group_consumed: false,
                target_terminated: false,
                terminal_session_ended: Some(false),
                cleanup_error: None,
                capture_error: None,
                error: None,
            },
        })
    }

    pub(super) fn start(&mut self, operation: &Operation) -> io::Result<()> {
        if self.status.started || self.status.terminal_session_ended == Some(true) {
            return Err(io::Error::other("execution already started or consumed"));
        }
        let Operation::Start {
            program,
            args,
            cwd,
            env,
            native_env,
            native_env_encoding,
            ..
        } = operation
        else {
            return Err(io::Error::other("expected Start"));
        };
        let environment = environment(env, native_env.as_deref(), native_env_encoding.as_deref())?;
        let mut command_line = command_line(program, args)?;
        let cwd = cwd
            .as_ref()
            .map(|value| wide(OsStr::new(value)))
            .transpose()?;
        // Opening the helper's inherited console explicitly avoids accidentally
        // redirecting the target to a control pipe. No new/detached console flag:
        // the target inherits this helper's ConPTY console attachment.
        let input = console("CONIN$", GENERIC_READ | GENERIC_WRITE)?;
        let output = console("CONOUT$", GENERIC_READ | GENERIC_WRITE)?;
        let handles = [input.0, output.0];
        let mut attribute_size = 0;
        unsafe { InitializeProcThreadAttributeList(null_mut(), 2, 0, &mut attribute_size) };
        let mut storage = vec![0usize; attribute_size.div_ceil(size_of::<usize>())];
        let attributes = storage.as_mut_ptr().cast();
        if unsafe { InitializeProcThreadAttributeList(attributes, 2, 0, &mut attribute_size) } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let result = (|| {
            if unsafe {
                UpdateProcThreadAttribute(
                    attributes,
                    0,
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                    handles.as_ptr().cast_mut().cast(),
                    size_of_val(&handles),
                    null_mut(),
                    null(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            let jobs = [self.job.0];
            // Atomic kernel job assignment closes even the helper-killed-between-
            // CreateProcess-and-AssignProcessToJobObject orphan window. ConPTY
            // already requires a Windows version supporting JOB_LIST.
            if unsafe {
                UpdateProcThreadAttribute(
                    attributes,
                    0,
                    PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                    jobs.as_ptr().cast_mut().cast(),
                    size_of_val(&jobs),
                    null_mut(),
                    null(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
            startup.StartupInfo.cb = size_of_val(&startup) as u32;
            startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            startup.StartupInfo.hStdInput = input.0;
            startup.StartupInfo.hStdOutput = output.0;
            startup.StartupInfo.hStdError = output.0;
            startup.lpAttributeList = attributes;
            let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
            if unsafe {
                CreateProcessW(
                    null(),
                    command_line.as_mut_ptr(),
                    null(),
                    null(),
                    1,
                    CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
                    environment.as_ptr().cast(),
                    cwd.as_ref().map_or(null(), |value| value.as_ptr()),
                    &startup.StartupInfo,
                    &mut process,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            let target = Handle::new(process.hProcess)?;
            let thread = Handle::new(process.hThread)?;
            // CreateProcess assigned the suspended target to our owned job
            // atomically; it cannot execute before this explicit resume.
            self.status.started = true;
            self.status.target_pid = Some(process.dwProcessId);
            self.target = Some(target);
            if unsafe { ResumeThread(thread.0) } == u32::MAX {
                let error = io::Error::last_os_error();
                self.sweep()?;
                return Err(error);
            }
            Ok(())
        })();
        unsafe { DeleteProcThreadAttributeList(attributes) };
        result
    }

    pub(super) fn sweep(&mut self) -> io::Result<()> {
        self.swept = true;
        if unsafe { TerminateJobObject(self.job.0, 1) } == 0 {
            let error = io::Error::last_os_error();
            self.status.cleanup_error = Some(error.to_string());
            return Err(error);
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            self.refresh()?;
            if self.status.terminal_session_ended == Some(true) {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                let error = io::Error::new(
                    io::ErrorKind::TimedOut,
                    "owned job termination remains unconfirmed",
                );
                self.status.cleanup_error = Some(error.to_string());
                return Err(error);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    pub(super) fn refresh(&mut self) -> io::Result<()> {
        if let Some(target) = &self.target
            && self.status.exit_code.is_none()
            && unsafe { WaitForSingleObject(target.0, 0) } == WAIT_OBJECT_0
        {
            let mut code = 0;
            if unsafe { GetExitCodeProcess(target.0, &mut code) } == 0 {
                return Err(io::Error::last_os_error());
            }
            // Preserve the full DWORD bit pattern in the protocol's i32.
            self.status.exit_code = Some(i32::from_ne_bytes(code.to_ne_bytes()));
            // Natural direct-target exit also ends its owned descendants.
            if !self.swept {
                self.sweep()?;
            }
        }
        let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe {
            QueryInformationJobObject(
                self.job.0,
                JobObjectBasicAccountingInformation,
                std::ptr::from_mut(&mut accounting).cast(),
                size_of_val(&accounting) as u32,
                null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if accounting.ActiveProcesses == 0
            && (self.status.exit_code.is_some() || self.swept)
            && (self.target.is_none() || self.status.exit_code.is_some())
        {
            self.status.terminal_session_ended = Some(true);
            self.status.group_consumed = self.swept;
            self.status.target_terminated = self.swept;
            self.status.cleanup_error = None;
        }
        Ok(())
    }
}

fn console(name: &str, access: u32) -> io::Result<Handle> {
    let name = wide(OsStr::new(name))?;
    let attributes = windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
        nLength: size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: null_mut(),
        bInheritHandle: 1,
    };
    Handle::new(unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            &attributes,
            OPEN_EXISTING,
            0,
            null_mut(),
        )
    })
}

fn decode(bytes: &[u8]) -> io::Result<OsString> {
    if !bytes.len().is_multiple_of(2) {
        return Err(io::Error::other("odd UTF-16LE environment entry"));
    }
    let units: Vec<_> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    if units.contains(&0) {
        return Err(io::Error::other("NUL in environment entry"));
    }
    Ok(OsString::from_wide(&units))
}
fn environment(
    env: &BTreeMap<String, String>,
    native: Option<&[(Vec<u8>, Vec<u8>)]>,
    encoding: Option<&str>,
) -> io::Result<Vec<u16>> {
    if !matches!(encoding, None | Some("windows-utf16le"))
        || native.is_some() && encoding != Some("windows-utf16le")
    {
        return Err(io::Error::other(
            "Windows native environment requires windows-utf16le",
        ));
    }
    let entries: Vec<_> = if let Some(entries) = native {
        entries
            .iter()
            .map(|(key, value)| Ok((decode(key)?, decode(value)?)))
            .collect::<io::Result<_>>()?
    } else {
        env.iter()
            .map(|(key, value)| (OsString::from(key), OsString::from(value)))
            .collect()
    };
    // Windows lookup and ordering are case-insensitive. Reject ambiguous keys;
    // retain original UTF-16, including legal unpaired native surrogates.
    let mut sorted: Vec<(Vec<u16>, Vec<u16>)> = Vec::new();
    for (key, value) in entries {
        let key = wide(&key)?;
        if !crate::windows_environment::valid_key(&key[..key.len() - 1]) {
            return Err(io::Error::other("invalid Windows environment key"));
        }
        let value = wide(&value)?;
        sorted.push((key, value));
    }
    // Use Windows' own ordinal case mapping, not Rust Unicode case expansion.
    // Lengths are bounded by the authenticated MAX_FRAME input limit.
    let compare = |left: &[u16], right: &[u16]| -> io::Result<std::cmp::Ordering> {
        let result = unsafe {
            windows_sys::Win32::Globalization::CompareStringOrdinal(
                left.as_ptr(),
                i32::try_from(left.len() - 1).map_err(io::Error::other)?,
                right.as_ptr(),
                i32::try_from(right.len() - 1).map_err(io::Error::other)?,
                1,
            )
        };
        match result {
            1 => Ok(std::cmp::Ordering::Less),
            2 => Ok(std::cmp::Ordering::Equal),
            3 => Ok(std::cmp::Ordering::Greater),
            _ => Err(io::Error::last_os_error()),
        }
    };
    let mut comparison_error = None;
    sorted.sort_by(|(left, _), (right, _)| {
        compare(left, right).unwrap_or_else(|error| {
            comparison_error = Some(error);
            std::cmp::Ordering::Equal
        })
    });
    if let Some(error) = comparison_error {
        return Err(error);
    }
    for pair in sorted.windows(2) {
        if compare(&pair[0].0, &pair[1].0)? == std::cmp::Ordering::Equal {
            return Err(io::Error::other("duplicate Windows environment key"));
        }
    }
    let mut result = Vec::new();
    for (key, value) in &sorted {
        result.extend_from_slice(&key[..key.len() - 1]);
        result.push(u16::from(b'='));
        result.extend(value);
    }
    if result.is_empty() {
        result.push(0);
    }
    result.push(0);
    Ok(result)
}

/// cmd.exe parses the remainder after /C or /K as shell source, not CRT argv.
/// A single explicit script is wrapped using /S semantics; its internal quotes
/// and backslashes must reach cmd unchanged. Other executables use CRT argv.
fn command_line(program: &str, args: &[String]) -> io::Result<Vec<u16>> {
    let mut result = quote(OsStr::new(program))?;
    let name = program.rsplit(['\\', '/']).next().unwrap_or(program);
    let cmd = name.eq_ignore_ascii_case("cmd.exe") || name.eq_ignore_ascii_case("cmd");
    let script_index = cmd
        .then(|| {
            args.iter()
                .position(|arg| arg.eq_ignore_ascii_case("/c") || arg.eq_ignore_ascii_case("/k"))
        })
        .flatten();
    if let Some(index) = script_index {
        if index + 2 != args.len() {
            return Err(io::Error::other(
                "cmd /C or /K requires one explicit script argument",
            ));
        }
        for flag in &args[..index] {
            if flag.contains([' ', '\t', '"']) || !flag.starts_with('/') {
                return Err(io::Error::other("invalid cmd option before script"));
            }
            result.push(u16::from(b' '));
            let flag = wide(OsStr::new(flag))?;
            result.extend_from_slice(&flag[..flag.len() - 1]);
        }
        result.extend(" /S ".encode_utf16());
        result.extend(args[index].encode_utf16());
        result.extend(" \"".encode_utf16());
        let script = wide(OsStr::new(&args[index + 1]))?;
        result.extend_from_slice(&script[..script.len() - 1]);
        result.push(u16::from(b'"'));
    } else {
        for argument in args {
            result.push(u16::from(b' '));
            result.extend(quote(OsStr::new(argument))?);
        }
    }
    result.push(0);
    Ok(result)
}

/// MS CRT argv quoting: backslashes are special only immediately before a quote.
fn quote(value: &OsStr) -> io::Result<Vec<u16>> {
    let value = wide(value)?;
    if value.len() > 1
        && !value[..value.len() - 1]
            .iter()
            .any(|unit| [u16::from(b' '), u16::from(b'\t'), u16::from(b'"')].contains(unit))
    {
        return Ok(value[..value.len() - 1].to_vec());
    }
    let mut output = vec![u16::from(b'"')];
    let mut slashes = 0;
    for &unit in &value[..value.len() - 1] {
        if unit == u16::from(b'\\') {
            slashes += 1;
            continue;
        }
        output.extend(std::iter::repeat_n(
            u16::from(b'\\'),
            if unit == u16::from(b'"') {
                slashes * 2 + 1
            } else {
                slashes
            },
        ));
        slashes = 0;
        output.push(unit);
    }
    output.extend(std::iter::repeat_n(u16::from(b'\\'), slashes * 2));
    output.push(u16::from(b'"'));
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quote_trailing_slash_and_embedded_quote() {
        assert_eq!(
            String::from_utf16(&quote(OsStr::new("a\\")).unwrap()).unwrap(),
            "a\\"
        );
        assert_eq!(
            String::from_utf16(&quote(OsStr::new("a\"b")).unwrap()).unwrap(),
            "\"a\\\"b\""
        );
    }
    #[test]
    fn cmd_script_is_not_crt_escaped() {
        let args = vec!["/D".into(), "/C".into(), "echo \"a b\" & exit /b 37".into()];
        let command = command_line(r"C:\Windows\System32\cmd.exe", &args).unwrap();
        assert_eq!(
            String::from_utf16(&command[..command.len() - 1]).unwrap(),
            "C:\\Windows\\System32\\cmd.exe /D /S /C \"echo \"a b\" & exit /b 37\""
        );
        assert!(
            command_line("cmd.exe", &["/C".into(), "echo".into(), "ambiguous".into()]).is_err()
        );
        assert_eq!(
            String::from_utf16(&quote(OsStr::new("/D")).unwrap()).unwrap(),
            "/D"
        );
        assert_eq!(
            String::from_utf16(&quote(OsStr::new("a b\\")).unwrap()).unwrap(),
            "\"a b\\\\\""
        );
    }
    #[test]
    fn empty_job_is_not_terminal_end_until_explicit_sweep() {
        let mut scope = Scope::new("test".into()).unwrap();
        scope.refresh().unwrap();
        assert_eq!(scope.status.terminal_session_ended, Some(false));
        scope.sweep().unwrap();
        assert_eq!(scope.status.terminal_session_ended, Some(true));
        assert!(!scope.status.started);
        assert_eq!(scope.status.exit_code, None);
        assert_eq!(scope.status.target_pid, None);
    }
    #[test]
    fn native_environment_preserves_surrogates_and_rejects_nul() {
        let native = vec![(vec![65, 0], vec![0, 0xd8])];
        assert_eq!(
            environment(&BTreeMap::new(), Some(&native), Some("windows-utf16le")).unwrap(),
            vec![65, 61, 0xd800, 0, 0]
        );
        assert!(decode(&[0]).is_err());
        assert!(decode(&[0, 0]).is_err());
    }
    #[test]
    fn drive_environment_keys_are_preserved_and_case_insensitive() {
        let env = BTreeMap::from([("=C:".into(), "C:\\work".into())]);
        let expected: Vec<_> = "=C:=C:\\work\0\0".encode_utf16().collect();
        assert_eq!(environment(&env, None, None).unwrap(), expected);
        let native = vec![(
            "=c:".encode_utf16().flat_map(u16::to_le_bytes).collect(),
            "C:\\native"
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect(),
        )];
        let expected: Vec<_> = "=c:=C:\\native\0\0".encode_utf16().collect();
        assert_eq!(
            environment(&BTreeMap::new(), Some(&native), Some("windows-utf16le")).unwrap(),
            expected
        );
        let duplicates =
            BTreeMap::from([("=C:".into(), "one".into()), ("=c:".into(), "two".into())]);
        assert!(environment(&duplicates, None, None).is_err());
        for key in ["", "=", "==", "=C:=", "A=B", "=ExitCode=Value"] {
            let env = BTreeMap::from([(key.into(), "value".into())]);
            assert!(environment(&env, None, None).is_err(), "accepted {key:?}");
        }
    }
    #[test]
    fn cmd_pseudo_environment_survives_both_transports() {
        let env = BTreeMap::from([
            ("=ExitCode".into(), "00000000".into()),
            ("=ExitCodeAscii".into(), "\u{20ac}".into()),
            ("=C:".into(), "C:\\work".into()),
            ("Path".into(), "C:\\Windows".into()),
        ]);
        let native: Vec<_> = env
            .iter()
            .map(|(key, value): (&String, &String)| {
                (
                    key.encode_utf16().flat_map(u16::to_le_bytes).collect(),
                    value.encode_utf16().flat_map(u16::to_le_bytes).collect(),
                )
            })
            .collect();
        let block = environment(&env, None, None).unwrap();
        assert_eq!(
            environment(&BTreeMap::new(), Some(&native), Some("windows-utf16le")).unwrap(),
            block
        );
        let text = String::from_utf16(&block).unwrap();
        assert!(text.contains("=ExitCode=00000000\0"));
        assert!(text.contains("=ExitCodeAscii=\u{20ac}\0"));
        let duplicates = BTreeMap::from([
            ("=ExitCode".into(), "0".into()),
            ("=exitcode".into(), "1".into()),
        ]);
        assert!(environment(&duplicates, None, None).is_err());
    }

    #[test]
    fn environment_rejects_wrong_platform_and_duplicates() {
        let native = vec![(vec![65, 0], vec![66, 0])];
        assert!(environment(&BTreeMap::new(), Some(&native), None).is_err());
        assert!(environment(&BTreeMap::new(), Some(&native), Some("unix-bytes")).is_err());
        assert_eq!(
            environment(&BTreeMap::new(), Some(&native), Some("windows-utf16le")).unwrap(),
            vec![65, 61, 66, 0, 0]
        );
        let env = BTreeMap::from([("Path".into(), "a".into()), ("PATH".into(), "b".into())]);
        assert!(environment(&env, None, None).is_err());
    }
}
