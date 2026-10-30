use std::io;
use std::path::Path;
use std::process::{Command, Stdio};

#[cfg(windows)]
const DETACHED_PROCESS: u32 = 0x0000_0008;
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Starts `exe args...` with no stdio and no console so it outlives the caller. On unix the child is
/// expected to call [`become_session_leader`] first thing; on Windows the creation flags detach it.
pub fn spawn_detached(exe: &Path, args: &[&str]) -> io::Result<()> {
    let mut command = Command::new(exe);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    command.spawn().map(drop)
}

/// Leaves the controlling terminal's session so a closed terminal cannot hang the daemon up. No-op on Windows.
pub fn become_session_leader() {
    #[cfg(unix)]
    {
        let _ = rustix::process::setsid();
    }
}
