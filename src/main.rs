#![windows_subsystem = "windows"]

mod accent;
mod alerts;
mod anim;
mod audio;
mod battery;
mod clock;
mod media;
mod model;
mod privacy;
mod settings;
mod ui;

use std::io::Write;

use windows::core::w;
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
use windows::Win32::System::Console::{AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

/// stdout if redirected, else the parent console (the exe is a GUI-subsystem binary).
fn console_out() -> Option<Box<dyn Write>> {
    // SAFETY: attaching to the parent console is harmless if there is none.
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        // Redirected stdout is inherited; otherwise write straight to the console.
        match GetStdHandle(STD_OUTPUT_HANDLE) {
            Ok(h) if !h.is_invalid() && !h.0.is_null() => Some(Box::new(std::io::stdout())),
            _ => std::fs::OpenOptions::new().write(true).open("CONOUT$").ok().map(|f| Box::new(f) as Box<dyn Write>),
        }
    }
}

fn dump_batteries() {
    let Some(mut out) = console_out() else { return };
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

fn dump_privacy() {
    let Some(mut out) = console_out() else { return };
    let p = privacy::scan();
    let _ = writeln!(out, "mic: {:?}", p.mic);
    let _ = writeln!(out, "cam: {:?}", p.cam);
}

fn main() {
    if std::env::args().any(|a| a == "--dump-batteries") {
        dump_batteries();
        return;
    }
    if std::env::args().any(|a| a == "--dump-privacy") {
        dump_privacy();
        return;
    }
    // SAFETY: process-wide setup before any window exists.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _mutex = CreateMutexW(None, false, w!(r"Local\isle-single-instance"));
        if GetLastError() == ERROR_ALREADY_EXISTS && !ui::test_knob("ISLE_TEST_NO_MUTEX") {
            return;
        }
        let _ = ui::run();
    }
}
