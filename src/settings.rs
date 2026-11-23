//! Registry-backed settings: window position and autostart.

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::*;

const KEY: PCWSTR = w!(r"Software\isle");
const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const RUN_VALUE: PCWSTR = w!("isle");

fn get_u32(name: PCWSTR) -> Option<u32> {
    let mut v = 0u32;
    let mut size = 4u32;
    // SAFETY: out pointers reference live locals of the right size.
    let r =
        unsafe { RegGetValueW(HKEY_CURRENT_USER, KEY, name, RRF_RT_REG_DWORD, None, Some(&mut v as *mut u32 as *mut _), Some(&mut size)) };
    (r == ERROR_SUCCESS).then_some(v)
}

fn set_u32(name: PCWSTR, v: u32) {
    // SAFETY: standard create/set/close sequence on a key we own.
    unsafe {
        let mut k = HKEY::default();
        if RegCreateKeyExW(HKEY_CURRENT_USER, KEY, None, PCWSTR::null(), REG_OPTION_NON_VOLATILE, KEY_SET_VALUE, None, &mut k, None)
            == ERROR_SUCCESS
        {
            let _ = RegSetValueExW(k, name, None, REG_DWORD, Some(&v.to_le_bytes()));
            let _ = RegCloseKey(k);
        }
    }
}

/// Saved pill top-left in physical pixels.
pub fn load_pos() -> Option<(i32, i32)> {
    Some((get_u32(w!("x"))? as i32, get_u32(w!("y"))? as i32))
}

pub fn save_pos(x: i32, y: i32) {
    set_u32(w!("x"), x as u32);
    set_u32(w!("y"), y as u32);
}

pub fn clear_pos() {
    // SAFETY: opens our own key, deletes two values.
    unsafe {
        let mut k = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, KEY, None, KEY_SET_VALUE, &mut k) == ERROR_SUCCESS {
            let _ = RegDeleteValueW(k, w!("x"));
            let _ = RegDeleteValueW(k, w!("y"));
            let _ = RegCloseKey(k);
        }
    }
}

pub fn autostart_enabled() -> bool {
    // SAFETY: size query only, no data pointer.
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, RRF_RT_REG_SZ, None, None, None) == ERROR_SUCCESS }
}

pub fn set_autostart(on: bool) {
    // SAFETY: standard open/set/delete on the per-user Run key.
    unsafe {
        let mut k = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, KEY_SET_VALUE, &mut k) != ERROR_SUCCESS {
            return;
        }
        if on {
            if let Ok(exe) = std::env::current_exe() {
                let s: Vec<u16> = format!("\"{}\"", exe.display()).encode_utf16().chain(Some(0)).collect();
                let bytes = std::slice::from_raw_parts(s.as_ptr() as *const u8, s.len() * 2);
                let _ = RegSetValueExW(k, RUN_VALUE, None, REG_SZ, Some(bytes));
            }
        } else {
            let _ = RegDeleteValueW(k, RUN_VALUE);
        }
        let _ = RegCloseKey(k);
    }
}

/// Auto-hide is the default; this flag keeps the island on screen permanently.
pub fn always_visible() -> bool {
    get_u32(w!("always_visible")).unwrap_or(0) != 0
}

pub fn set_always_visible(on: bool) {
    set_u32(w!("always_visible"), on as u32);
}
