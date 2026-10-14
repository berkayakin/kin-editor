//! Starting other programs.

use std::{ffi::OsStr, process::Command};

/// A command that opens no console window. Kin is a GUI app on Windows, so
/// each console program it starts (git, shells, servers, compilers) would
/// otherwise flash a window of its own.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}
