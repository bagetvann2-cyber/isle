//! Self-update from GitHub releases: check, download, verify, swap the exe, restart.
//!
//! Security model: HTTPS only (WinHTTP), redirects are followed by hand and every hop must stay on a GitHub host,
//! and the new exe is installed only if its SHA-256 equals the `digest` GitHub published for the release asset.
//! No threads or timers here; callers run `check` / `download_and_install` from a worker thread.
#![allow(dead_code)] // the island banner / menu wiring lands in a later change

use core::ffi::c_void;
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Networking::WinHttp::*;
use windows::Win32::Security::Cryptography::*;

const LATEST_URL: &str = "https://api.github.com/repos/bagetvann2-cyber/isle/releases/latest";
const PAGE_PREFIX: &str = "https://github.com/bagetvann2-cyber/isle/";
const DOWNLOAD_PREFIX: &str = "https://github.com/bagetvann2-cyber/isle/releases/download/";
const ASSET_NAME: &str = "isle.exe";
const ALLOWED_HOSTS: [&str; 4] = ["api.github.com", "github.com", "objects.githubusercontent.com", "release-assets.githubusercontent.com"];
const MAX_JSON: usize = 1 << 20;
const MAX_EXE: usize = 20 << 20;
const MAX_REDIRECTS: usize = 5;
const MAX_URL: usize = 2048;
const TOTAL_DEADLINE: Duration = Duration::from_secs(300);
/// Flag passed to the restarted process so it waits for the old instance to release the single-instance mutex.
pub const AFTER_UPDATE_FLAG: &str = "--after-update";

// ---------------------------------------------------------------------------------------------- errors

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// WinHTTP / OS networking failure (text from the OS).
    Network(String),
    /// Server answered with a non-success status.
    Http(u32),
    /// Release JSON is not what we expect.
    Parse(&'static str),
    /// A URL is not https, not on an allowed host, or malformed.
    BadUrl,
    TooLarge,
    Timeout,
    /// The release carries no SHA-256 digest, so it is never installed.
    NoChecksum,
    HashMismatch,
    NotExe,
    Io(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Network(e) => write!(f, "network error: {e}"),
            Error::Http(s) => write!(f, "server returned HTTP {s}"),
            Error::Parse(what) => write!(f, "unexpected release data: {what}"),
            Error::BadUrl => f.write_str("refusing a URL outside the allowed GitHub hosts"),
            Error::TooLarge => f.write_str("response is too large"),
            Error::Timeout => f.write_str("timed out"),
            Error::NoChecksum => f.write_str("release has no checksum, not installing"),
            Error::HashMismatch => f.write_str("downloaded file does not match its checksum"),
            Error::NotExe => f.write_str("downloaded file is not a Windows executable"),
            Error::Io(e) => write!(f, "file error: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<windows::core::Error> for Error {
    fn from(e: windows::core::Error) -> Self {
        Error::Network(e.to_string())
    }
}

fn io(e: std::io::Error) -> Error {
    Error::Io(e.to_string())
}

// -------------------------------------------------------------------------------------------- versions

/// `major.minor.patch`; a pre-release suffix (`-beta`) sorts below the plain release. Build metadata is ignored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    stable: bool, // last, so `false` (pre-release) < `true` for equal numbers
}

impl Version {
    /// Accepts `1.2.3`, `v1.2.3`, `1.2.3-beta.1`, `1.2.3+build`. Anything else is `None`.
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim();
        let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
        let (core, stable) = match s.find(['-', '+']) {
            Some(i) => (&s[..i], s.as_bytes()[i] == b'+'),
            None => (s, true),
        };
        let mut it =
            core.split('.').map(|p| if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) { p.parse::<u64>().ok() } else { None });
        let (major, minor, patch) = (it.next()??, it.next()??, it.next()??);
        if it.next().is_some() {
            return None;
        }
        Some(Version { major, minor, patch, stable })
    }

    /// The running build's version (0.0.0-pre if Cargo's version were ever malformed).
    pub fn current() -> Version {
        Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or(Version { major: 0, minor: 0, patch: 0, stable: false })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}{}", self.major, self.minor, self.patch, if self.stable { "" } else { "-pre" })
    }
}

pub fn is_newer(remote: &Version, current: &Version) -> bool {
    remote > current
}

// ----------------------------------------------------------------------------------------- release JSON

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    pub tag: String,
    pub exe_url: String,
    pub sha256: Option<[u8; 32]>,
    pub page_url: String,
}

/// `sha256:<64 hex>` (the form GitHub uses in an asset's `digest`) or bare hex.
fn parse_sha256_hex(s: &str) -> Option<[u8; 32]> {
    let hex = s.strip_prefix("sha256:").unwrap_or(s);
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

pub fn parse_latest(json: &str) -> Result<Release, Error> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|_| Error::Parse("invalid JSON"))?;
    let tag = v["tag_name"].as_str().filter(|t| t.len() <= 64).ok_or(Error::Parse("tag_name"))?;
    let version = Version::parse(tag).ok_or(Error::Parse("tag_name is not a version"))?;
    let page_url = v["html_url"].as_str().ok_or(Error::Parse("html_url"))?;
    let asset = v["assets"].as_array().and_then(|a| a.iter().find(|x| x["name"] == ASSET_NAME)).ok_or(Error::Parse("no isle.exe asset"))?;
    let exe_url = asset["browser_download_url"].as_str().ok_or(Error::Parse("browser_download_url"))?;
    if !page_url.starts_with(PAGE_PREFIX) || !exe_url.starts_with(DOWNLOAD_PREFIX) {
        return Err(Error::BadUrl);
    }
    split_url(page_url)?;
    split_url(exe_url)?;
    let sha256 = asset["digest"].as_str().and_then(parse_sha256_hex);
    Ok(Release { version, tag: tag.to_string(), exe_url: exe_url.to_string(), sha256, page_url: page_url.to_string() })
}

// ------------------------------------------------------------------------------------------------- HTTP

/// `(host, path-and-query)` of an https URL whose host is on the allow-list. Rejects ports, userinfo, odd bytes.
fn split_url(url: &str) -> Result<(&str, &str), Error> {
    if url.len() > MAX_URL || !url.bytes().all(|b| (0x21..=0x7e).contains(&b) && b != b'\\') {
        return Err(Error::BadUrl);
    }
    let rest = url.strip_prefix("https://").ok_or(Error::BadUrl)?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = &rest[..end];
    if !ALLOWED_HOSTS.iter().any(|h| h.eq_ignore_ascii_case(host)) {
        return Err(Error::BadUrl);
    }
    let path = rest[end..].split('#').next().unwrap_or("");
    Ok((host, if path.is_empty() || path.starts_with('?') { "/" } else { path }))
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

struct Handle(*mut c_void);

impl Handle {
    fn open(h: *mut c_void, what: &str) -> Result<Handle, Error> {
        if h.is_null() {
            Err(Error::Network(format!("{what}: {}", windows::core::Error::from_thread())))
        } else {
            Ok(Handle(h))
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful WinHttp* open call and is closed exactly once.
        unsafe {
            let _ = WinHttpCloseHandle(self.0);
        }
    }
}

/// GET with hand-followed redirects: every hop is checked against the host allow-list *before* it is requested.
/// No cookies, no auth, no extra headers; the body is capped at `limit` bytes.
fn get(url: &str, accept: &str, limit: usize) -> Result<Vec<u8>, Error> {
    let deadline = Instant::now() + TOTAL_DEADLINE;
    let ua = wide(&format!("isle/{}", env!("CARGO_PKG_VERSION")));
    let accept = wide(accept);
    // SAFETY: all pointers passed below point to live, NUL-terminated buffers or locals that outlive the calls.
    unsafe {
        let session = Handle::open(
            WinHttpOpen(PCWSTR(ua.as_ptr()), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, PCWSTR::null(), PCWSTR::null(), 0),
            "WinHttpOpen",
        )?;
        // Best effort: older Windows 10 builds reject the TLS 1.3 bit, the system default is fine there.
        let protocols = WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2 | WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_3;
        let _ = WinHttpSetOption(Some(session.0), WINHTTP_OPTION_SECURE_PROTOCOLS, Some(&protocols.to_le_bytes()));
        WinHttpSetTimeouts(session.0, 10_000, 10_000, 10_000, 10_000)?;

        let mut cur = url.to_string();
        for _ in 0..=MAX_REDIRECTS {
            let (host, path) = split_url(&cur)?;
            let host_w = wide(host);
            let path_w = wide(path);
            let connect = Handle::open(WinHttpConnect(session.0, PCWSTR(host_w.as_ptr()), 443, 0), "WinHttpConnect")?;
            let types = [PCWSTR(accept.as_ptr()), PCWSTR::null()];
            let req = Handle::open(
                WinHttpOpenRequest(
                    connect.0,
                    w!("GET"),
                    PCWSTR(path_w.as_ptr()),
                    PCWSTR::null(),
                    PCWSTR::null(),
                    types.as_ptr(),
                    WINHTTP_FLAG_SECURE,
                ),
                "WinHttpOpenRequest",
            )?;
            let off = WINHTTP_DISABLE_COOKIES | WINHTTP_DISABLE_REDIRECTS | WINHTTP_DISABLE_AUTHENTICATION;
            WinHttpSetOption(Some(req.0), WINHTTP_OPTION_DISABLE_FEATURE, Some(&off.to_le_bytes()))?;
            WinHttpSendRequest(req.0, None, None, 0, 0, 0)?;
            WinHttpReceiveResponse(req.0, std::ptr::null_mut())?;

            let mut status = 0u32;
            let mut len = 4u32;
            WinHttpQueryHeaders(
                req.0,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                PCWSTR::null(),
                Some(&mut status as *mut u32 as *mut c_void),
                &mut len,
                std::ptr::null_mut(),
            )?;
            match status {
                200 => return read_body(&req, limit, deadline),
                301 | 302 | 303 | 307 | 308 => {
                    let mut buf = [0u16; 4096];
                    let mut len = std::mem::size_of_val(&buf) as u32;
                    WinHttpQueryHeaders(
                        req.0,
                        WINHTTP_QUERY_LOCATION,
                        PCWSTR::null(),
                        Some(buf.as_mut_ptr() as *mut c_void),
                        &mut len,
                        std::ptr::null_mut(),
                    )?;
                    cur = String::from_utf16(&buf[..len as usize / 2]).map_err(|_| Error::BadUrl)?;
                }
                s => return Err(Error::Http(s)),
            }
        }
        Err(Error::Http(310)) // too many redirects
    }
}

/// # Safety
/// `req` must be a request handle whose response headers have been received.
unsafe fn read_body(req: &Handle, limit: usize, deadline: Instant) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    loop {
        if Instant::now() > deadline {
            return Err(Error::Timeout);
        }
        let mut avail = 0u32;
        WinHttpQueryDataAvailable(req.0, &mut avail)?;
        if avail == 0 {
            return Ok(out);
        }
        let n = avail.min(64 * 1024) as usize;
        if out.len() + n > limit {
            return Err(Error::TooLarge);
        }
        let start = out.len();
        out.resize(start + n, 0);
        let mut read = 0u32;
        WinHttpReadData(req.0, out[start..].as_mut_ptr() as *mut c_void, n as u32, &mut read)?;
        out.truncate(start + read as usize);
        if read == 0 {
            return Ok(out);
        }
    }
}

// ------------------------------------------------------------------------------------------------ SHA-256

fn sha256(data: &[u8]) -> Result<[u8; 32], Error> {
    fn ok(s: windows::Win32::Foundation::NTSTATUS, what: &'static str) -> Result<(), Error> {
        if s.0 >= 0 {
            Ok(())
        } else {
            Err(Error::Network(format!("{what} failed: 0x{:08x}", s.0 as u32)))
        }
    }
    // SAFETY: BCrypt handles are created and destroyed within this function; buffers are live slices.
    unsafe {
        let mut alg = BCRYPT_ALG_HANDLE::default();
        ok(
            BCryptOpenAlgorithmProvider(&mut alg, BCRYPT_SHA256_ALGORITHM, PCWSTR::null(), BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS(0)),
            "BCryptOpenAlgorithmProvider",
        )?;
        let mut hash = BCRYPT_HASH_HANDLE::default();
        let mut out = [0u8; 32];
        let r = ok(BCryptCreateHash(alg, &mut hash, None, None, 0), "BCryptCreateHash").and_then(|_| {
            let r = ok(BCryptHashData(hash, data, 0), "BCryptHashData")
                .and_then(|_| ok(BCryptFinishHash(hash, &mut out, 0), "BCryptFinishHash"));
            let _ = BCryptDestroyHash(hash);
            r
        });
        let _ = BCryptCloseAlgorithmProvider(alg, 0);
        r.map(|_| out)
    }
}

// ----------------------------------------------------------------------------------------------- install

static EXE: OnceLock<Option<PathBuf>> = OnceLock::new();

/// The running exe's path, captured on first use (`cleanup_old` does it at startup) so that it keeps
/// meaning "isle.exe" even after the file on disk has been swapped.
fn exe_path() -> Result<PathBuf, Error> {
    EXE.get_or_init(|| std::env::current_exe().ok()).clone().ok_or_else(|| Error::Io("cannot locate the running exe".into()))
}

/// Check GitHub for a release strictly newer than `current`. `Ok(None)` also covers "no stable release yet"
/// (`/releases/latest` answers 404 when only pre-releases exist).
pub fn check(current: &Version) -> Result<Option<Release>, Error> {
    let body = match get(LATEST_URL, "application/vnd.github+json", MAX_JSON) {
        Err(Error::Http(404)) => return Ok(None),
        r => r?,
    };
    let json = String::from_utf8(body).map_err(|_| Error::Parse("response is not UTF-8"))?;
    let rel = parse_latest(&json)?;
    Ok(is_newer(&rel.version, current).then_some(rel))
}

/// Download `rel`, verify it against the published SHA-256 and replace the running exe. The caller restarts.
pub fn download_and_install(rel: &Release) -> Result<(), Error> {
    let want = rel.sha256.ok_or(Error::NoChecksum)?;
    // The URL must be exactly the isle.exe asset of this tag, and the release must really be newer than us.
    if rel.exe_url != format!("{DOWNLOAD_PREFIX}{}/{ASSET_NAME}", rel.tag) {
        return Err(Error::BadUrl);
    }
    if !is_newer(&rel.version, &Version::current()) {
        return Err(Error::Parse("release is not newer than the running build"));
    }
    let exe = exe_path()?;
    let bytes = get(&rel.exe_url, "application/octet-stream", MAX_EXE)?;
    stage_and_swap(&exe, &bytes, &want)
}

/// Verify `bytes`, write them next to `exe` as `isle.new.exe`, re-verify what landed on disk, then swap.
fn stage_and_swap(exe: &Path, bytes: &[u8], want: &[u8; 32]) -> Result<(), Error> {
    if sha256(bytes)? != *want {
        return Err(Error::HashMismatch);
    }
    if !bytes.starts_with(b"MZ") {
        return Err(Error::NotExe);
    }
    let name = exe.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name.eq_ignore_ascii_case("isle.new.exe") || name.eq_ignore_ascii_case("isle.old.exe") {
        return Err(Error::Io("refusing to update an exe named like a staging file".into()));
    }
    let new = exe.with_file_name("isle.new.exe");
    let old = exe.with_file_name("isle.old.exe");
    // Flush to disk before the rename: a power cut must not leave a truncated isle.exe behind.
    let write = std::fs::File::create(&new).and_then(|mut f| f.write_all(bytes).and_then(|_| f.sync_all()));
    write.map_err(|e| {
        let _ = std::fs::remove_file(&new);
        io(e)
    })?;
    // The file could have been touched between our write and the rename; check what is actually on disk.
    let on_disk = std::fs::read(&new).map_err(io).and_then(|b| sha256(&b));
    if on_disk.as_ref() != Ok(want) {
        let _ = std::fs::remove_file(&new);
        return Err(on_disk.err().unwrap_or(Error::HashMismatch));
    }
    swap(exe, &new, &old)
}

/// `exe` -> `old`, `new` -> `exe`. On any failure `exe` is back in place and `new` is deleted
/// (except when even the rollback fails: then both files are kept).
/// (A running exe can be renamed on Windows; `fs::rename` replaces an existing target.)
fn swap(exe: &Path, new: &Path, old: &Path) -> Result<(), Error> {
    if let Err(e) = std::fs::rename(exe, old) {
        let _ = std::fs::remove_file(new);
        return Err(io(e));
    }
    if let Err(e) = std::fs::rename(new, exe) {
        return Err(match std::fs::rename(old, exe) {
            Ok(()) => {
                let _ = std::fs::remove_file(new);
                io(e)
            }
            // Keep the verified new exe: with isle.old.exe it is all the user has left to recover from.
            Err(r) => Error::Io(format!("{e}; rollback also failed: {r} (isle.old.exe and isle.new.exe kept)")),
        });
    }
    Ok(())
}

/// Call at startup: remove leftovers of a previous update (`isle.old.exe`, a crashed `isle.new.exe`).
pub fn cleanup_old() {
    if let Ok(exe) = exe_path() {
        for name in ["isle.old.exe", "isle.new.exe"] {
            let p = exe.with_file_name(name);
            if p != exe {
                let _ = std::fs::remove_file(p);
            }
        }
    }
}

/// Start the (new) exe with the same arguments plus `--after-update`; the caller should exit right after `Ok`.
pub fn restart() -> Result<(), Error> {
    let exe = exe_path()?;
    let args = std::env::args_os().skip(1).filter(|a| a != AFTER_UPDATE_FLAG);
    Command::new(exe)
        .args(args)
        .arg(AFTER_UPDATE_FLAG)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(io)
}

// -------------------------------------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn newer_matrix() {
        assert!(is_newer(&v("0.2.10"), &v("0.2.1")));
        assert!(!is_newer(&v("0.2.1"), &v("0.2.10")));
        assert!(is_newer(&v("v0.3.0"), &v("0.2.9")));
        assert!(is_newer(&v("1.0.0"), &v("0.99.99")));
        assert!(!is_newer(&v("0.2.1"), &v("0.2.1")));
        assert!(!is_newer(&v("v0.2.1"), &v("0.2.1")));
        assert!(!is_newer(&v("0.1.9"), &v("0.2.0")));
        // pre-release is older than the release, newer than the previous release
        assert!(is_newer(&v("0.3.0"), &v("0.3.0-beta")));
        assert!(!is_newer(&v("0.3.0-beta"), &v("0.3.0")));
        assert!(is_newer(&v("0.3.0-beta"), &v("0.2.9")));
        assert!(!is_newer(&v("0.3.0-beta.2"), &v("0.3.0-beta.1"))); // same numbers: not offered
        assert_eq!(v("0.3.0+build5"), v("0.3.0"));
    }

    #[test]
    fn version_parse_rejects_garbage() {
        for s in ["", "1", "1.2", "1.2.3.4", "a.b.c", "1..3", "1.2.-3", "+1.2.3", "1.2.3x", "latest"] {
            assert!(Version::parse(s).is_none(), "{s:?}");
        }
        assert_eq!(v(" v1.2.3 ").to_string(), "1.2.3");
        assert_eq!(v("1.2.3-rc1").to_string(), "1.2.3-pre");
        assert!(Version::current() > Version::parse("0.0.0-pre").unwrap());
    }

    const HEX: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn sample(digest: Option<&str>, asset: &str, url: &str, html: &str) -> String {
        let digest = digest.map(|d| format!(r#","digest":"{d}""#)).unwrap_or_default();
        format!(
            r#"{{"url":"https://api.github.com/repos/bagetvann2-cyber/isle/releases/1","html_url":"{html}","tag_name":"v0.3.0","name":"isle 0.3.0",
            "draft":false,"prerelease":false,"assets":[
              {{"name":"isle.exe.sha256","size":80,"browser_download_url":"{DOWNLOAD_PREFIX}v0.3.0/isle.exe.sha256"}},
              {{"name":"{asset}","size":1234567,"content_type":"application/x-msdownload","browser_download_url":"{url}"{digest}}}]}}"#
        )
    }

    const HTML: &str = "https://github.com/bagetvann2-cyber/isle/releases/tag/v0.3.0";
    const EXE_URL: &str = "https://github.com/bagetvann2-cyber/isle/releases/download/v0.3.0/isle.exe";

    #[test]
    fn parse_latest_ok() {
        let r = parse_latest(&sample(Some(&format!("sha256:{HEX}")), "isle.exe", EXE_URL, HTML)).unwrap();
        assert_eq!(r.version, v("0.3.0"));
        assert_eq!(r.tag, "v0.3.0");
        assert_eq!(r.exe_url, EXE_URL);
        assert_eq!(r.page_url, HTML);
        assert_eq!(r.sha256, parse_sha256_hex(HEX));
        assert!(r.sha256.is_some());
    }

    #[test]
    fn parse_latest_without_digest() {
        let r = parse_latest(&sample(None, "isle.exe", EXE_URL, HTML)).unwrap();
        assert_eq!(r.sha256, None);
        // a malformed digest is treated as absent, never as a wrong-but-accepted value
        let r = parse_latest(&sample(Some("sha256:zz"), "isle.exe", EXE_URL, HTML)).unwrap();
        assert_eq!(r.sha256, None);
        assert_eq!(download_and_install(&r), Err(Error::NoChecksum));
    }

    #[test]
    fn parse_latest_rejects() {
        let d = format!("sha256:{HEX}");
        assert_eq!(parse_latest(&sample(Some(&d), "isle-setup.exe", EXE_URL, HTML)), Err(Error::Parse("no isle.exe asset")));
        for bad in [
            "https://evil.example/bagetvann2-cyber/isle/releases/download/v0.3.0/isle.exe",
            "http://github.com/bagetvann2-cyber/isle/releases/download/v0.3.0/isle.exe",
            "https://github.com.evil.example/bagetvann2-cyber/isle/releases/download/v0.3.0/isle.exe",
            "https://github.com/other/isle/releases/download/v0.3.0/isle.exe",
            "https://github.com/bagetvann2-cyber/isle/releases/download/v0.3.0/isle.exe evil",
        ] {
            assert_eq!(parse_latest(&sample(Some(&d), "isle.exe", bad, HTML)), Err(Error::BadUrl), "{bad}");
        }
        assert_eq!(parse_latest(&sample(Some(&d), "isle.exe", EXE_URL, "https://evil.example/x")), Err(Error::BadUrl));
        assert!(parse_latest("not json").is_err());
        assert!(parse_latest("{}").is_err());
        assert!(parse_latest(r#"{"tag_name":"nightly","html_url":"","assets":[]}"#).is_err());
    }

    #[test]
    fn url_allow_list() {
        assert_eq!(split_url("https://github.com/a/b?x=1#frag"), Ok(("github.com", "/a/b?x=1")));
        assert_eq!(split_url("https://api.github.com"), Ok(("api.github.com", "/")));
        assert_eq!(split_url("https://objects.githubusercontent.com?x=1"), Ok(("objects.githubusercontent.com", "/")));
        for bad in [
            "http://github.com/",
            "https://github.com:444/",
            "https://user@github.com/",
            "https://github.com@evil.example/",
            "https://evil.example/",
            "https://githubusercontent.com/",
            "https://github.com\\@evil.example/",
            "//github.com/",
            "/relative",
        ] {
            assert_eq!(split_url(bad), Err(Error::BadUrl), "{bad}");
        }
    }

    #[test]
    fn sha256_hex_and_digest() {
        assert_eq!(sha256(b"abc").unwrap(), parse_sha256_hex(HEX).unwrap());
        assert_eq!(parse_sha256_hex(&format!("sha256:{}", HEX.to_uppercase())), parse_sha256_hex(HEX));
        assert_eq!(sha256(b"").unwrap(), parse_sha256_hex("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855").unwrap());
        assert_eq!(parse_sha256_hex(&HEX[..63]), None);
        assert_eq!(parse_sha256_hex(&format!("{HEX}0")), None);
        assert_eq!(parse_sha256_hex("sha512:abc"), None);
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let d = std::env::temp_dir().join(format!("isle-update-test-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn stage_and_swap_installs_and_keeps_old() {
        let d = temp_dir("ok");
        let exe = d.join("isle.exe");
        std::fs::write(&exe, b"MZ-old").unwrap();
        let new_bytes = b"MZ-new-build";
        stage_and_swap(&exe, new_bytes, &sha256(new_bytes).unwrap()).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), new_bytes);
        assert_eq!(std::fs::read(d.join("isle.old.exe")).unwrap(), b"MZ-old");
        assert!(!d.join("isle.new.exe").exists());
        // a second update replaces the previous isle.old.exe
        let newer = b"MZ-newer-build";
        stage_and_swap(&exe, newer, &sha256(newer).unwrap()).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), newer);
        assert_eq!(std::fs::read(d.join("isle.old.exe")).unwrap(), new_bytes);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn stage_and_swap_refuses_bad_payloads() {
        let d = temp_dir("bad");
        let exe = d.join("isle.exe");
        std::fs::write(&exe, b"MZ-old").unwrap();
        assert_eq!(stage_and_swap(&exe, b"MZ-x", &[0u8; 32]), Err(Error::HashMismatch));
        let text = b"<html>not an exe</html>";
        assert_eq!(stage_and_swap(&exe, text, &sha256(text).unwrap()), Err(Error::NotExe));
        assert_eq!(std::fs::read(&exe).unwrap(), b"MZ-old");
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1); // nothing staged, nothing left over
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn swap_rolls_back_when_second_rename_fails() {
        let d = temp_dir("rollback");
        let exe = d.join("isle.exe");
        std::fs::write(&exe, b"MZ-old").unwrap();
        // `new` does not exist, so rename(new, exe) fails after exe was already moved aside.
        let r = swap(&exe, &d.join("isle.new.exe"), &d.join("isle.old.exe"));
        assert!(matches!(r, Err(Error::Io(_))));
        assert_eq!(std::fs::read(&exe).unwrap(), b"MZ-old");
        assert!(!d.join("isle.old.exe").exists());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn swap_failure_on_first_rename_leaves_everything_intact() {
        let d = temp_dir("first");
        let exe = d.join("isle.exe"); // missing: first rename fails
        let new = d.join("isle.new.exe");
        std::fs::write(&new, b"MZ-new").unwrap();
        assert!(swap(&exe, &new, &d.join("isle.old.exe")).is_err());
        assert!(!new.exists());
        std::fs::remove_dir_all(d).unwrap();
    }

    /// Windows lets you rename an image that is currently executing; this is what the real update relies on.
    #[test]
    fn swap_works_while_the_exe_is_running() {
        let sys = std::env::var_os("SystemRoot").map(|r| PathBuf::from(r).join("System32").join("ping.exe"));
        let Some(sys) = sys.filter(|p| p.exists()) else { return };
        let d = temp_dir("running");
        let exe = d.join("isle.exe");
        std::fs::copy(&sys, &exe).unwrap();
        let mut child = Command::new(&exe).args(["-n", "6", "127.0.0.1"]).stdout(Stdio::null()).spawn().unwrap();
        let mut payload = b"MZ".to_vec();
        payload.extend_from_slice(b"replacement");
        let r = stage_and_swap(&exe, &payload, &sha256(&payload).unwrap());
        let _ = child.kill();
        let _ = child.wait();
        r.unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), payload);
        assert!(d.join("isle.old.exe").exists());
        std::fs::remove_dir_all(d).unwrap();
    }
}
