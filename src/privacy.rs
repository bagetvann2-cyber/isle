//! Which apps are using the microphone / camera right now, from the per-user
//! CapabilityAccessManager consent store. Event-driven: one thread parked in
//! `WaitForMultipleObjects` on registry change notifications, zero polling.
// TODO(coder2): remove when wired
#![allow(dead_code)]

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{ERROR_SUCCESS, FILETIME, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Registry::*;
use windows::Win32::System::SystemInformation::{GetSystemTimeAsFileTime, GetTickCount64};
use windows::Win32::System::Threading::{CreateEventW, WaitForMultipleObjects};

const STORE: &str = r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore";
const MAX_NAMES: usize = 3;
const DEBOUNCE_MS: u32 = 300;
const MAX_BURST_MS: u64 = 2_000;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PrivacyState {
    pub mic: Vec<String>,
    pub cam: Vec<String>,
}

// ---- pure helpers (unit-tested) ----

/// Package family name "Microsoft.WindowsCamera_8wekyb3d8bbwe" -> "Microsoft.WindowsCamera".
fn name_from_pfn(pfn: &str) -> String {
    pfn.split('_').next().unwrap_or(pfn).to_string()
}

/// Desktop app key "C:#Program Files#Foo#foo.exe" -> "foo".
fn name_from_path(key: &str) -> String {
    let file = key.rsplit(['#', '\\', '/']).next().unwrap_or(key);
    match file.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem.to_string(),
        _ => file.to_string(),
    }
}

/// FILETIME of the last boot, from the current FILETIME and `GetTickCount64` (ms).
fn boot_filetime(now_ft: u64, tick_ms: u64) -> u64 {
    now_ft.saturating_sub(tick_ms.saturating_mul(10_000))
}

/// In use = started, not stopped, and started after boot (entries older than that are
/// stale leftovers of a hard power-off).
fn in_use(start: u64, stop: u64, boot_ft: u64) -> bool {
    start != 0 && stop == 0 && start >= boot_ft
}

/// Dedupe, sort, cap.
fn finish(mut names: Vec<String>) -> Vec<String> {
    names.sort();
    names.dedup();
    names.truncate(MAX_NAMES);
    names
}

// ---- registry ----

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn read_qword(path: &str, name: PCWSTR) -> Option<u64> {
    let p = wide(path);
    let mut v = 0u64;
    let mut size = 8u32;
    // SAFETY: `p` is NUL-terminated and outlives the call; out pointers reference live locals.
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(p.as_ptr()),
            name,
            RRF_RT_REG_QWORD,
            None,
            Some(&mut v as *mut u64 as *mut _),
            Some(&mut size),
        )
    };
    (r == ERROR_SUCCESS).then_some(v)
}

fn subkeys(path: &str) -> Vec<String> {
    let p = wide(path);
    let mut out = Vec::new();
    // SAFETY: standard open / enumerate / close; `buf` and `len` are live locals.
    unsafe {
        let mut k = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(p.as_ptr()), None, KEY_READ, &mut k) != ERROR_SUCCESS {
            return out;
        }
        let mut buf = [0u16; 512];
        for i in 0.. {
            let mut len = buf.len() as u32;
            if RegEnumKeyExW(k, i, Some(PWSTR(buf.as_mut_ptr())), &mut len, None, None, None, None) != ERROR_SUCCESS {
                break;
            }
            out.push(String::from_utf16_lossy(&buf[..len as usize]));
        }
        let _ = RegCloseKey(k);
    }
    out
}

fn now_filetime() -> u64 {
    // SAFETY: plain query, no pointers in.
    let ft: FILETIME = unsafe { GetSystemTimeAsFileTime() };
    (ft.dwHighDateTime as u64) << 32 | ft.dwLowDateTime as u64
}

/// Apps using one capability ("microphone" / "webcam").
fn scan_capability(cap: &str, boot_ft: u64, own: &str) -> Vec<String> {
    let base = format!(r"{STORE}\{cap}");
    let mut names = Vec::new();
    let mut check = |sub: &str, display: String| {
        let path = format!(r"{base}\{sub}");
        let start = read_qword(&path, w!("LastUsedTimeStart")).unwrap_or(0);
        let stop = read_qword(&path, w!("LastUsedTimeStop")).unwrap_or(0);
        if in_use(start, stop, boot_ft) && !display.eq_ignore_ascii_case(own) {
            names.push(display);
        }
    };
    for k in subkeys(&base) {
        if k == "NonPackaged" {
            for app in subkeys(&format!(r"{base}\NonPackaged")) {
                check(&format!(r"NonPackaged\{app}"), name_from_path(&app));
            }
        } else {
            check(&k, name_from_pfn(&k));
        }
    }
    finish(names)
}

pub fn scan() -> PrivacyState {
    // SAFETY: plain query.
    let boot = boot_filetime(now_filetime(), unsafe { GetTickCount64() });
    let own = std::env::current_exe().ok().and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned())).unwrap_or_default();
    PrivacyState { mic: scan_capability("microphone", boot, &own), cam: scan_capability("webcam", boot, &own) }
}

// ---- watcher thread ----

struct Watch {
    key: HKEY,
    event: HANDLE,
}

fn arm(w: &Watch) -> bool {
    // SAFETY: key and event are live for the thread's lifetime.
    unsafe {
        RegNotifyChangeKeyValue(w.key, true, REG_NOTIFY_CHANGE_LAST_SET | REG_NOTIFY_CHANGE_NAME, Some(w.event), true) == ERROR_SUCCESS
    }
}

fn open_watch(cap: &str) -> Option<Watch> {
    let p = wide(&format!(r"{STORE}\{cap}"));
    // SAFETY: standard open + auto-reset event; both live until process exit.
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(p.as_ptr()), None, KEY_NOTIFY | KEY_READ, &mut key) != ERROR_SUCCESS {
            return None;
        }
        let event = CreateEventW(None, false, false, None).ok()?;
        let w = Watch { key, event };
        arm(&w).then_some(w)
    }
}

/// Start the watcher: `on_change` runs once with the initial state, then on every change.
pub fn spawn(on_change: impl Fn(PrivacyState) + Send + 'static) {
    let _ = std::thread::Builder::new().name("privacy".into()).spawn(move || {
        // Arm first, scan second: a change in between still signals.
        let mut watches: Vec<Watch> = ["microphone", "webcam"].into_iter().filter_map(open_watch).collect();
        let mut last = scan();
        on_change(last.clone());
        while !watches.is_empty() {
            let events: Vec<HANDLE> = watches.iter().map(|w| w.event).collect();
            // Index of the signalled event, `None` on timeout / failure.
            // SAFETY: handles are valid events owned by this thread.
            let wait = |ms: u32| {
                let i = unsafe { WaitForMultipleObjects(&events, false, ms).0.wrapping_sub(WAIT_OBJECT_0.0) } as usize;
                (i < events.len()).then_some(i)
            };
            let Some(first) = wait(u32::MAX) else { return }; // WAIT_FAILED: give up quietly
                                                              // Coalesce a burst (re-arm only what fired: a second registration on a key that
                                                              // is still pending would just pile up), but never hold the scan back past MAX_BURST_MS.
            let started = unsafe { GetTickCount64() };
            let mut fired = Some(first);
            let mut dead = None;
            while let Some(i) = fired {
                if !arm(&watches[i]) {
                    dead = Some(i); // key deleted / handle gone: stop watching it
                    break;
                }
                if unsafe { GetTickCount64() } - started >= MAX_BURST_MS {
                    break;
                }
                fired = wait(DEBOUNCE_MS);
            }
            if let Some(i) = dead {
                watches.remove(i);
            }
            let now = scan();
            if now != last {
                last = now.clone();
                on_change(now);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pfn_names() {
        assert_eq!(name_from_pfn("Microsoft.WindowsCamera_8wekyb3d8bbwe"), "Microsoft.WindowsCamera");
        assert_eq!(name_from_pfn("Plain"), "Plain");
        assert_eq!(name_from_pfn("_x"), "");
    }

    #[test]
    fn path_names() {
        assert_eq!(name_from_path("C:#Program Files#Zoom#bin#Zoom.exe"), "Zoom");
        assert_eq!(name_from_path(r"C:\Windows\System32\foo.bar.exe"), "foo.bar");
        assert_eq!(name_from_path("noext"), "noext");
        assert_eq!(name_from_path("C:#x#.hidden"), ".hidden");
    }

    #[test]
    fn boot_filter() {
        let now = 133_000_000_000_000_000u64;
        let boot = boot_filetime(now, 60_000); // up 60 s
        assert_eq!(boot, now - 600_000_000);
        assert!(in_use(boot + 1, 0, boot));
        assert!(in_use(boot, 0, boot));
        assert!(!in_use(boot - 1, 0, boot)); // stale: started before boot
        assert!(!in_use(0, 0, boot)); // never used
        assert!(!in_use(boot + 5, boot + 10, boot)); // stopped
        assert_eq!(boot_filetime(5, u64::MAX), 0); // no overflow
    }

    #[test]
    fn finish_dedupes_sorts_caps() {
        let v = ["z", "b", "a", "b", "d"].map(String::from).to_vec();
        assert_eq!(finish(v), vec!["a", "b", "d"]);
        assert!(finish(vec![]).is_empty());
    }

    /// Needs a real registry: writes a fake desktop-app entry under HKCU, expects the
    /// watcher to report it and then to clear it, and removes the entry.
    /// Run with `cargo test --release -- --ignored live_watcher`.
    #[test]
    #[ignore]
    fn live_watcher_sees_registry_change() {
        use std::sync::mpsc::channel;
        use std::time::{Duration, Instant};
        let p = wide(&format!(r"{STORE}\microphone\NonPackaged\C:#isle-test#fakeapp.exe"));
        let set = |name: PCWSTR, v: u64| unsafe {
            let mut k = HKEY::default();
            let r = RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(p.as_ptr()),
                None,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                None,
                &mut k,
                None,
            );
            assert_eq!(r, ERROR_SUCCESS);
            let _ = RegSetValueExW(k, name, None, REG_QWORD, Some(&v.to_le_bytes()));
            let _ = RegCloseKey(k);
        };
        let (tx, rx) = channel();
        spawn(move |s| {
            let _ = tx.send(s);
        });
        let wait = |want: bool| {
            let end = Instant::now() + Duration::from_secs(5);
            while let Ok(s) = rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
                if s.mic.iter().any(|n| n == "fakeapp") == want {
                    return true;
                }
            }
            false
        };
        assert!(wait(false), "initial state");
        set(w!("LastUsedTimeStop"), 0);
        set(w!("LastUsedTimeStart"), now_filetime());
        let seen = wait(true);
        set(w!("LastUsedTimeStop"), now_filetime() + 1);
        let cleared = wait(false);
        // SAFETY: deletes the key created above.
        unsafe {
            let _ = RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(p.as_ptr()));
        }
        assert!(seen, "in-use not reported");
        assert!(cleared, "stop not reported");
    }
}
