#![windows_subsystem = "windows"]

mod accent;
mod anim;
mod audio;
mod battery;
mod media;
mod model;
mod settings;
mod ui;

use std::io::Write;

use windows::core::w;
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
use windows::Win32::System::Console::{AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

fn dump_batteries() {
    // SAFETY: attaching to the parent console is harmless if there is none.
    let out: Box<dyn Write> = unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        // Redirected stdout is inherited; otherwise write straight to the console.
        match GetStdHandle(STD_OUTPUT_HANDLE) {
            Ok(h) if !h.is_invalid() && !h.0.is_null() => Box::new(std::io::stdout()),
            _ => match std::fs::OpenOptions::new().write(true).open("CONOUT$") {
                Ok(f) => Box::new(f),
                Err(_) => return,
            },
        }
    };
    let mut out = out;
    for d in battery::snapshot_now() {
        let _ = writeln!(
            out,
            "{:<10} {:>3}%  {}  [{}]{}",
            format!("{:?}", d.kind),
            d.percent,
            d.name,
            d.id,
            if d.charging == Some(true) { "  charging" } else { "" }
        );
    }
}

fn main() {
    if std::env::args().any(|a| a == "--dump-batteries") {
        dump_batteries();
        return;
    }
    // SAFETY: process-wide setup before any window exists.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _mutex = CreateMutexW(None, false, w!(r"Local\isle-single-instance"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }
        let _ = ui::run();
    }
}
