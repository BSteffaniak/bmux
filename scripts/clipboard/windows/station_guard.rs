// Inserted into a TEST COPY of the shared interop example (see README.md).
// Check kernel object identity before BMUX code, never by reading a clipboard.
fn verify_test_desktop() -> Result<(), &'static str> {
    use std::ffi::c_void;
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetProcessWindowStation() -> *mut c_void;
        fn GetThreadDesktop(thread: u32) -> *mut c_void;
        fn GetUserObjectInformationW(
            object: *mut c_void,
            index: i32,
            info: *mut c_void,
            len: u32,
            needed: *mut u32,
        ) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThreadId() -> u32;
    }
    fn name(obj: *mut c_void) -> Result<String, &'static str> {
        let mut buf = [0u16; 128];
        let mut used = 0;
        // SAFETY: writable fixed-size UTF-16 buffer; handle is borrowed.
        if unsafe { GetUserObjectInformationW(obj, 2, buf.as_mut_ptr().cast(), 256, &mut used) }
            == 0
        {
            return Err("cannot query user object");
        }
        let n = buf
            .iter()
            .position(|&x| x == 0)
            .ok_or("unterminated name")?;
        String::from_utf16(&buf[..n]).map_err(|_| "invalid name")
    }
    let station = std::env::var("BMUX_WIN_TEST_STATION").map_err(|_| "no test station")?;
    if !station
        .strip_prefix("BmuxTest_")
        .is_some_and(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err("not a test station");
    }
    // SAFETY: query this process and its calling thread only. UOI_FLAGS is
    // USEROBJECTFLAGS (two BOOLs then a DWORD); WSF_VISIBLE is bit zero.
    unsafe {
        let ws = GetProcessWindowStation();
        let mut flags = [0u32; 3];
        let mut used = 0;
        if GetUserObjectInformationW(ws, 1, flags.as_mut_ptr().cast(), 12, &mut used) == 0
            || flags[2] & 1 != 0
            || name(ws)? != station
            || name(GetThreadDesktop(GetCurrentThreadId()))? != "BmuxDesk"
        {
            return Err("wrong window station/desktop or visible station");
        }
    }
    Ok(())
}
