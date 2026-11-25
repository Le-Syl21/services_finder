//! Small Win32 helpers: wide strings, error text, elevation, clipboard.

use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{CloseHandle, GlobalFree, HANDLE, HWND};
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Diagnostics::Debug::{
    FORMAT_MESSAGE_FROM_SYSTEM, FORMAT_MESSAGE_IGNORE_INSERTS, FormatMessageW,
};
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// UTF-16, NUL-terminated copy of `s`.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Reads a NUL-terminated UTF-16 string; null gives an empty string.
pub fn from_wide_ptr(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut n = 0;
    // SAFETY: callers pass pointers to NUL-terminated strings owned by a live buffer.
    unsafe {
        while *p.add(n) != 0 {
            n += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
    }
}

/// System message for a Win32 error code, in the user's language.
pub fn error_text(code: u32) -> String {
    let mut buf = [0u16; 512];
    // SAFETY: the buffer and its length match; no inserts are used.
    let n = unsafe {
        FormatMessageW(
            FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
            null(),
            code,
            0,
            buf.as_mut_ptr(),
            buf.len() as u32,
            null(),
        )
    };
    let msg = String::from_utf16_lossy(&buf[..n as usize])
        .trim()
        .to_string();
    if msg.is_empty() {
        format!("Error {code}")
    } else {
        format!("{msg} ({code})")
    }
}

/// True when this process runs with an elevated (administrator) token.
pub fn is_elevated() -> bool {
    let mut token: HANDLE = null_mut();
    // SAFETY: the pseudo-handle of the current process is always valid.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let mut e = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut len = 0u32;
    // SAFETY: `e` is the structure this information class fills.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&mut e as *mut TOKEN_ELEVATION).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
    };
    // SAFETY: `token` was opened above.
    unsafe { CloseHandle(token) };
    ok != 0 && e.TokenIsElevated != 0
}

/// Puts `text` on the clipboard as Unicode text.
pub fn set_clipboard(hwnd: HWND, text: &str) {
    const CF_UNICODETEXT: u32 = 13;
    let w = wide(text);
    // SAFETY: standard clipboard sequence; the global block is handed to the
    // system on success and freed by us otherwise.
    unsafe {
        if OpenClipboard(hwnd) == 0 {
            return;
        }
        EmptyClipboard();
        let mem = GlobalAlloc(GMEM_MOVEABLE, w.len() * 2);
        if !mem.is_null() {
            let p = GlobalLock(mem) as *mut u16;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
                GlobalUnlock(mem);
                if SetClipboardData(CF_UNICODETEXT, mem).is_null() {
                    GlobalFree(mem);
                }
            } else {
                GlobalFree(mem);
            }
        }
        CloseClipboard();
    }
}

/// Executable path out of a service's command line (quoted or not), with
/// environment variables expanded.
pub fn binary_from_command(cmd: &str) -> String {
    let cmd = cmd.trim();
    let exe = if let Some(rest) = cmd.strip_prefix('"') {
        rest.split('"').next().unwrap_or(rest).to_string()
    } else {
        let lower = cmd.to_ascii_lowercase();
        match lower.find(".exe") {
            Some(i) => cmd[..i + 4].to_string(),
            None => cmd.split(' ').next().unwrap_or(cmd).to_string(),
        }
    };
    expand_env(&exe)
}

fn expand_env(s: &str) -> String {
    use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;
    let w = wide(s);
    let mut buf = vec![0u16; 1024];
    // SAFETY: input is NUL-terminated and the output length matches `buf`.
    let n = unsafe { ExpandEnvironmentStringsW(w.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    if n == 0 || n as usize > buf.len() {
        return s.to_string();
    }
    from_wide_ptr(buf.as_ptr())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_paths() {
        assert_eq!(
            binary_from_command(r#""C:\Program Files\App\svc.exe" -run"#),
            r"C:\Program Files\App\svc.exe"
        );
        assert_eq!(
            binary_from_command(r"C:\x\svchost.exe -k netsvcs"),
            r"C:\x\svchost.exe"
        );
    }
}
