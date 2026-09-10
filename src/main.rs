//! # orca-tui (binary)
//!
//! Thin entry point. All parsing and dispatch lives in the [`orca_tui`]
//! library crate's [`cli`](orca_tui::cli) module; the binary just runs it and
//! maps the result to a process exit code.

use std::process::ExitCode;

fn main() -> ExitCode {
    // Install the crash-logging panic hook FIRST, before anything that can
    // panic, so an edge-case crash (e.g. shrinking the window past a layout
    // underflow) leaves a detailed report in last-crash.log instead of dying
    // silently.
    orca_tui::crashlog::install();
    match orca_tui::cli::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("orca-tui: {err:#}");
            ExitCode::FAILURE
        }
    }
}
