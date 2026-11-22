//! Windows session authority: a kill-on-close Job Object per pane.
//!
//! The pane's program is assigned to the job as it is created, so it and
//! everything it starts stay inside. The server holds the only handle: if it
//! dies, the kernel closes the handle and kill-on-close ends the pane's
//! processes, which is why Windows needs no crash records.
use std::io;
use std::os::windows::io::{AsHandle as _, AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::time::Duration;
use switchy_thread::sync::Mutex;
use switchy_time::Instant;
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

/// How long a sweep waits for the job's processes to end.
const SWEEP_TIMEOUT: Duration = Duration::from_secs(2);
/// The exit code of processes ended by a sweep.
const SWEPT_EXIT_CODE: u32 = 1;

/// The pane's job, which must exist before the spawn so the program starts in it.
#[derive(Debug)]
pub(crate) struct Preparation {
    job: OwnedHandle,
}

pub(crate) fn prepare() -> anyhow::Result<Preparation> {
    Ok(Preparation { job: job()? })
}

impl Preparation {
    /// Spawn the pane's program on `slave`, inside the job from its creation.
    pub(crate) fn spawn(
        &self,
        slave: &bmux_pty::PtySlave,
        command: &bmux_pty::SpawnCommand,
    ) -> io::Result<Box<dyn crate::runtime::pty::Child + Send + Sync>> {
        Ok(Box::new(slave.spawn_in_job(command, self.job.as_handle())?))
    }
}

/// An unnamed, non-inheritable job that ends its processes when its last
/// handle closes. Without breakaway flags nothing started inside can leave it.
fn job() -> io::Result<OwnedHandle> {
    // SAFETY: an unnamed job with default (non-inheritable) security.
    let job = unsafe { CreateJobObjectW(null(), null()) };
    if job.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateJobObjectW returned a new handle that nothing else owns.
    let job = unsafe { OwnedHandle::from_raw_handle(job) };
    set_kill_on_close(&job, true)?;
    Ok(job)
}

fn set_kill_on_close(job: &OwnedHandle, kill: bool) -> io::Result<()> {
    // SAFETY: zeroed is a valid empty limit structure.
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    if kill {
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    }
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
    Ok(())
}

fn active_processes(job: &OwnedHandle) -> io::Result<u32> {
    // SAFETY: zeroed is a valid empty accounting structure.
    let mut accounting: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the structure outlives the call and its size is passed.
    if unsafe {
        QueryInformationJobObject(
            job.as_raw_handle(),
            JobObjectBasicAccountingInformation,
            (&raw mut accounting).cast(),
            u32::try_from(size_of_val(&accounting)).unwrap_or(u32::MAX),
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(accounting.ActiveProcesses)
}

/// Whether process `pid` belongs to `job`.
fn contains(job: &OwnedHandle, pid: u32) -> io::Result<bool> {
    // SAFETY: OpenProcess has no memory-safety preconditions.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: OpenProcess returned a new handle that nothing else owns.
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    let mut result = 0;
    // SAFETY: both handles are open and the out-pointer is valid.
    if unsafe {
        IsProcessInJob(
            process.as_raw_handle(),
            job.as_raw_handle(),
            &raw mut result,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(result != 0)
}

#[derive(Debug)]
pub(crate) struct Session {
    /// `None` once released.
    job: Mutex<Option<OwnedHandle>>,
}

impl Session {
    pub(crate) fn adopt(
        preparation: Preparation,
        leader: u32,
        _master: &bmux_pty::PtyMaster,
    ) -> anyhow::Result<Self> {
        // The caller still holds the leader's process handle, so its pid
        // cannot have been reused, even if it already exited.
        anyhow::ensure!(
            contains(&preparation.job, leader)?,
            "pane process {leader} is not inside its job"
        );
        Ok(Self {
            job: Mutex::new(Some(preparation.job)),
        })
    }

    /// The job's kill-on-close covers a crashed server; nothing to record.
    #[allow(clippy::unnecessary_wraps, clippy::unused_self)] // Unix records sessions.
    pub(crate) const fn commit_record(
        &self,
        _root: &Path,
        _token: uuid::Uuid,
    ) -> anyhow::Result<Option<PathBuf>> {
        Ok(None)
    }

    /// Cooperative stop is unsupported on Windows; the caller escalates.
    #[allow(clippy::unnecessary_wraps, clippy::unused_self)] // Unix signals the leader.
    pub(crate) const fn terminate_leader(&self, _leader: u32) -> io::Result<bool> {
        Ok(false)
    }

    fn with_job<T>(&self, operation: impl FnOnce(&OwnedHandle) -> io::Result<T>) -> io::Result<T> {
        let job = self
            .job
            .lock()
            .map_err(|_| io::Error::other("pane job lock poisoned"))?;
        let job = job
            .as_ref()
            .ok_or_else(|| io::Error::other("pane job already released"))?;
        operation(job)
    }

    /// End every process in the job and wait until none remain.
    pub(crate) fn sweep(&self) -> io::Result<()> {
        self.with_job(|job| {
            // SAFETY: an open job handle.
            if unsafe { TerminateJobObject(job.as_raw_handle(), SWEPT_EXIT_CODE) } == 0 {
                return Err(io::Error::last_os_error());
            }
            // Termination is asynchronous. Completion-port notifications are
            // not guaranteed to be delivered, so the process count decides.
            let deadline = Instant::now() + SWEEP_TIMEOUT;
            loop {
                let remaining = active_processes(job)?;
                if remaining == 0 {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "{remaining} process(es) remain in the pane's job after termination"
                        ),
                    ));
                }
                switchy_thread::thread::sleep(Duration::from_millis(10));
            }
        })
    }

    pub(crate) fn occupied(&self) -> io::Result<bool> {
        self.with_job(|job| Ok(active_processes(job)? > 0))
    }

    /// Clear kill-on-close so releasing the job leaves its processes running.
    pub(crate) fn release_survivors(&self) {
        if let Err(error) = self.with_job(|job| set_kill_on_close(job, false)) {
            tracing::warn!(%error, "could not release the pane job's remaining processes");
        }
    }

    pub(crate) fn release(&self) {
        if let Ok(mut job) = self.job.lock() {
            job.take();
        }
    }
}

/// Windows writes no crash records.
#[allow(clippy::unnecessary_wraps)] // Unix sweeps the recorded session.
/// Nothing to do: the session's job closed with the preparation adoption consumed, and its
/// kill-on-close ended every member.
#[allow(clippy::unnecessary_wraps)] // Unix sweeps the session, which can fail.
pub(crate) const fn sweep_unadopted(_leader: u32) -> std::io::Result<()> {
    Ok(())
}

pub(crate) const fn recover_record(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_job_is_unoccupied_and_sweeps_cleanly() {
        let session = Session {
            job: Mutex::new(Some(job().unwrap())),
        };
        assert!(!session.occupied().unwrap());
        session.sweep().unwrap();
        assert!(!session.occupied().unwrap());
        session.release_survivors();
        session.release();
        session.release();
        assert!(session.sweep().is_err());
        assert!(session.occupied().is_err());
    }

    #[test]
    fn records_are_not_used_on_windows() {
        let session = Session {
            job: Mutex::new(Some(job().unwrap())),
        };
        let root = tempfile::tempdir().unwrap();
        assert!(
            session
                .commit_record(root.path(), crate::ids::new_uuid())
                .unwrap()
                .is_none()
        );
        assert!(!session.terminate_leader(1).unwrap());
        recover_record(root.path()).unwrap();
    }

    /// A console whose cursor query is answered, so its first process runs.
    fn console() -> bmux_pty::PtyPair {
        use std::io::{Read as _, Write as _};
        let pair = bmux_pty::PtyPair::open(bmux_pty::PtySize::new(80, 24)).unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let mut input = pair.master.take_writer().unwrap();
        switchy_thread::thread::spawn(move || {
            let mut buffer = [0; 4096];
            while let Ok(read) = reader.read(&mut buffer) {
                if read == 0 {
                    return;
                }
                if buffer[..read].windows(4).any(|window| window == b"\x1b[6n") {
                    let _ = input.write_all(b"\x1b[1;1R");
                    let _ = input.flush();
                }
            }
        });
        pair
    }

    fn long_running() -> bmux_pty::SpawnCommand {
        let mut command = bmux_pty::SpawnCommand::new(
            std::env::var_os("COMSPEC").unwrap_or_else(|| r"C:\Windows\System32\cmd.exe".into()),
        );
        // cmd starts ping as a descendant, which must stay inside the job.
        command.args(["/D", "/C", "ping -n 60 127.0.0.1 >nul"]);
        command
    }

    #[test]
    fn a_sweep_ends_the_leader_and_its_descendants() {
        let pair = console();
        let preparation = prepare().unwrap();
        let mut child = preparation.spawn(&pair.slave, &long_running()).unwrap();
        let leader = child.process_id().unwrap();
        let session = Session::adopt(preparation, leader, &pair.master).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while session.with_job(active_processes).unwrap() < 2 {
            assert!(Instant::now() < deadline, "ping never started");
            switchy_thread::thread::sleep(Duration::from_millis(10));
        }
        assert!(session.occupied().unwrap());
        session.sweep().unwrap();
        assert!(!session.occupied().unwrap());
        assert_eq!(child.wait().unwrap().exit_code(), SWEPT_EXIT_CODE);
        session.release();
    }

    #[test]
    fn released_survivors_outlive_the_job_handle() {
        let pair = console();
        let preparation = prepare().unwrap();
        let mut child = preparation.spawn(&pair.slave, &long_running()).unwrap();
        let session =
            Session::adopt(preparation, child.process_id().unwrap(), &pair.master).unwrap();
        session.release_survivors();
        session.release();
        switchy_thread::thread::sleep(Duration::from_millis(200));
        assert!(
            child.try_wait().unwrap().is_none(),
            "kill-on-close still fired"
        );
        crate::runtime::pty::ChildKiller::kill(child.as_mut()).unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn closing_the_job_ends_its_processes() {
        let pair = console();
        let preparation = prepare().unwrap();
        let mut child = preparation.spawn(&pair.slave, &long_running()).unwrap();
        let session =
            Session::adopt(preparation, child.process_id().unwrap(), &pair.master).unwrap();
        session.release();
        let deadline = Instant::now() + Duration::from_secs(10);
        while child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "kill-on-close did not fire");
            switchy_thread::thread::sleep(Duration::from_millis(10));
        }
    }
}
