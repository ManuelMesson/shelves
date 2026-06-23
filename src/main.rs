#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]

use std::process::ExitCode;

fn main() -> ExitCode {
    restore_sigpipe_default();
    shelves::cli::main_entry()
}

#[cfg(unix)]
fn restore_sigpipe_default() {
    const SIGPIPE: i32 = 13;
    const SIG_DFL: usize = 0;

    unsafe extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }

    unsafe {
        signal(SIGPIPE, SIG_DFL);
    }
}

#[cfg(not(unix))]
fn restore_sigpipe_default() {}
