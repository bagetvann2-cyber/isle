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
mod update;

use std::io::Write;

use windows::core::w;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
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

fn check_update() {
    let Some(mut out) = console_out() else { return };
    let cur = update::Version::current();
    let _ = writeln!(out, "current: {cur}");
    match update::check(&cur) {
        Ok(Some(r)) => {
            let _ = writeln!(out, "update available: {} (tag {})", r.version, r.tag);
            let _ = writeln!(out, "  page:   {}", r.page_url);
            let _ = writeln!(out, "  exe:    {}", r.exe_url);
            let _ = writeln!(out, "  sha256: {}", if r.sha256.is_some() { "published" } else { "MISSING (would not install)" });
        }
        Ok(None) => {
            let _ = writeln!(out, "up to date (or no stable release published)");
        }
        Err(e) => {
            let _ = writeln!(out, "check failed: {e}");
        }
    }
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
    if std::env::args().any(|a| a == "--check-update") {
        check_update();
        return;
    }
    // SAFETY: process-wide setup before any window exists.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let create = || (CreateMutexW(None, false, w!(r"Local\isle-single-instance")), GetLastError() == ERROR_ALREADY_EXISTS);
        let (mut mutex, mut taken) = create();
        // After an update the old instance may still be shutting down: wait for it to release the mutex.
        if taken && std::env::args().any(|a| a == update::AFTER_UPDATE_FLAG) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while taken && std::time::Instant::now() < deadline {
                // Our handle to the existing mutex would keep it alive; drop it before retrying.
                if let Ok(h) = mutex {
                    let _ = CloseHandle(h);
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
                (mutex, taken) = create();
            }
        }
        let _mutex = mutex;
        if taken && !ui::test_knob("ISLE_TEST_NO_MUTEX") {
            return;
        }
        // Keep isle.old.exe right after an update (manual rollback); the next normal start removes it.
        if !std::env::args().any(|a| a == update::AFTER_UPDATE_FLAG) {
            update::cleanup_old();
        }
        let _ = ui::run();
    }
}
