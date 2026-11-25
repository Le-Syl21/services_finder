//! Service Control Manager access: enumeration, configuration, access probes
//! and the actions the window (or the elevated helper) performs.

use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_INSUFFICIENT_BUFFER, ERROR_MORE_DATA, ERROR_SERVICE_NOT_ACTIVE,
    ERROR_TIMEOUT, GetLastError,
};
use windows_sys::Win32::System::Services::*;

use crate::util::{from_wide_ptr, wide};

/// How a service is configured to start.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StartType {
    Boot,
    System,
    Auto,
    AutoDelayed,
    Manual,
    Disabled,
    Unknown,
}

impl StartType {
    /// Order used when sorting by the startup type column.
    pub fn rank(self) -> u8 {
        match self {
            StartType::Boot => 0,
            StartType::System => 1,
            StartType::Auto => 2,
            StartType::AutoDelayed => 3,
            StartType::Manual => 4,
            StartType::Disabled => 5,
            StartType::Unknown => 6,
        }
    }
}

/// One service as shown in the list.
#[derive(Clone, Debug)]
pub struct Service {
    pub name: String,
    pub display: String,
    pub description: String,
    pub path: String,
    pub account: String,
    pub state: u32,
    pub pid: u32,
    pub start: StartType,
    /// Launched as a protected process (anti-malware and the like): even an
    /// administrator cannot stop it.
    pub protected: bool,
    /// The current token may start and stop it without elevation.
    pub can_control: bool,
    /// The current token may change its configuration without elevation.
    pub can_config: bool,
}

/// What the user asked to do with a service.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Start,
    Stop,
    Restart,
    SetStart(StartType),
}

impl Action {
    /// Command-line spelling, used to hand the action to an elevated copy.
    pub fn as_arg(self) -> &'static str {
        match self {
            Action::Start => "start",
            Action::Stop => "stop",
            Action::Restart => "restart",
            Action::SetStart(StartType::Auto) => "auto",
            Action::SetStart(StartType::AutoDelayed) => "delayed",
            Action::SetStart(StartType::Manual) => "manual",
            Action::SetStart(StartType::Disabled) => "disabled",
            Action::SetStart(_) => "unknown",
        }
    }

    pub fn from_arg(s: &str) -> Option<Action> {
        Some(match s {
            "start" => Action::Start,
            "stop" => Action::Stop,
            "restart" => Action::Restart,
            "auto" => Action::SetStart(StartType::Auto),
            "delayed" => Action::SetStart(StartType::AutoDelayed),
            "manual" => Action::SetStart(StartType::Manual),
            "disabled" => Action::SetStart(StartType::Disabled),
            _ => return None,
        })
    }

    /// Whether this action needs start/stop rights (otherwise config rights).
    pub fn is_control(self) -> bool {
        !matches!(self, Action::SetStart(_))
    }
}

/// Owned SC_HANDLE, closed on drop.
struct Handle(SC_HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the handle came from OpenSCManagerW/OpenServiceW and is closed once.
            unsafe { CloseServiceHandle(self.0) };
        }
    }
}

fn open_manager(access: u32) -> Result<Handle, u32> {
    // SAFETY: null machine and database names select the local active database.
    let h = unsafe { OpenSCManagerW(null(), null(), access) };
    if h.is_null() {
        // SAFETY: plain thread-local error read.
        Err(unsafe { GetLastError() })
    } else {
        Ok(Handle(h))
    }
}

fn open_service(scm: &Handle, name: &[u16], access: u32) -> Result<Handle, u32> {
    // SAFETY: `name` is NUL-terminated and `scm` is a live manager handle.
    let h = unsafe { OpenServiceW(scm.0, name.as_ptr(), access) };
    if h.is_null() {
        // SAFETY: plain thread-local error read.
        Err(unsafe { GetLastError() })
    } else {
        Ok(Handle(h))
    }
}

/// An 8-byte aligned scratch buffer for the variable-size SCM structures.
fn aligned(bytes: u32) -> Vec<u64> {
    vec![0u64; (bytes as usize).div_ceil(8).max(1)]
}

/// Calls a "query into caller buffer" function, growing the buffer as asked.
fn query_buffer(mut call: impl FnMut(*mut u8, u32, &mut u32) -> i32) -> Option<Vec<u64>> {
    let mut needed = 0u32;
    if call(null_mut(), 0, &mut needed) != 0 {
        return Some(aligned(0));
    }
    // SAFETY: plain thread-local error read.
    let err = unsafe { GetLastError() };
    if err != ERROR_INSUFFICIENT_BUFFER || needed == 0 {
        return None;
    }
    let mut buf = aligned(needed);
    let len = (buf.len() * 8) as u32;
    if call(buf.as_mut_ptr().cast(), len, &mut needed) != 0 {
        Some(buf)
    } else {
        None
    }
}

/// Name, display name, state and process id of every Win32 service.
pub fn enumerate_states() -> Result<Vec<(String, String, u32, u32)>, u32> {
    let scm = open_manager(SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE)?;
    let mut out = Vec::new();
    let mut buf = aligned(64 * 1024);
    let mut resume = 0u32;
    loop {
        let mut needed = 0u32;
        let mut count = 0u32;
        let len = (buf.len() * 8) as u32;
        // SAFETY: buffer pointer/length describe `buf`; out-params are valid locals.
        let ok = unsafe {
            EnumServicesStatusExW(
                scm.0,
                SC_ENUM_PROCESS_INFO,
                SERVICE_WIN32,
                SERVICE_STATE_ALL,
                buf.as_mut_ptr().cast(),
                len,
                &mut needed,
                &mut count,
                &mut resume,
                null(),
            )
        };
        // SAFETY: plain thread-local error read.
        let err = if ok == 0 {
            unsafe { GetLastError() }
        } else {
            0
        };
        let entries = buf.as_ptr() as *const ENUM_SERVICE_STATUS_PROCESSW;
        for i in 0..count as usize {
            // SAFETY: the API wrote `count` entries at the start of `buf`; their
            // string pointers point inside `buf`, which is still alive.
            let e = unsafe { &*entries.add(i) };
            out.push((
                from_wide_ptr(e.lpServiceName),
                from_wide_ptr(e.lpDisplayName),
                e.ServiceStatusProcess.dwCurrentState,
                e.ServiceStatusProcess.dwProcessId,
            ));
        }
        if ok != 0 {
            return Ok(out);
        }
        if err != ERROR_MORE_DATA {
            return Err(err);
        }
        if needed as usize > buf.len() * 8 {
            buf = aligned(needed);
        }
    }
}

/// Every Win32 service with its configuration and the access the current
/// token has to it.
pub fn enumerate() -> Result<Vec<Service>, u32> {
    let states = enumerate_states()?;
    let scm = open_manager(SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE)?;
    let mut out = Vec::with_capacity(states.len());
    for (name, display, state, pid) in states {
        let w = wide(&name);
        let mut s = Service {
            name,
            display,
            description: String::new(),
            path: String::new(),
            account: String::new(),
            state,
            pid,
            start: StartType::Unknown,
            protected: false,
            can_control: open_service(&scm, &w, SERVICE_START | SERVICE_STOP).is_ok(),
            can_config: open_service(&scm, &w, SERVICE_CHANGE_CONFIG).is_ok(),
        };
        if let Ok(h) = open_service(&scm, &w, SERVICE_QUERY_CONFIG) {
            read_config(&h, &mut s);
        }
        out.push(s);
    }
    Ok(out)
}

fn read_config(h: &Handle, s: &mut Service) {
    let cfg = query_buffer(|p, len, needed| {
        // SAFETY: `p`/`len` describe a writable buffer (or null/0 to size it).
        unsafe { QueryServiceConfigW(h.0, p.cast(), len, needed) }
    });
    let mut auto = false;
    if let Some(buf) = cfg {
        // SAFETY: the call succeeded, so `buf` starts with a QUERY_SERVICE_CONFIGW.
        let c = unsafe { &*(buf.as_ptr() as *const QUERY_SERVICE_CONFIGW) };
        s.path = from_wide_ptr(c.lpBinaryPathName);
        s.account = from_wide_ptr(c.lpServiceStartName);
        s.start = match c.dwStartType {
            SERVICE_BOOT_START => StartType::Boot,
            SERVICE_SYSTEM_START => StartType::System,
            SERVICE_AUTO_START => {
                auto = true;
                StartType::Auto
            }
            SERVICE_DEMAND_START => StartType::Manual,
            SERVICE_DISABLED => StartType::Disabled,
            _ => StartType::Unknown,
        };
    }
    if let Some(buf) = config2(h, SERVICE_CONFIG_DESCRIPTION) {
        // SAFETY: the call succeeded for this info level.
        let d = unsafe { &*(buf.as_ptr() as *const SERVICE_DESCRIPTIONW) };
        s.description = from_wide_ptr(d.lpDescription);
    }
    if auto {
        if let Some(buf) = config2(h, SERVICE_CONFIG_DELAYED_AUTO_START_INFO) {
            // SAFETY: the call succeeded for this info level.
            let d = unsafe { &*(buf.as_ptr() as *const SERVICE_DELAYED_AUTO_START_INFO) };
            if d.fDelayedAutostart != 0 {
                s.start = StartType::AutoDelayed;
            }
        }
    }
    if let Some(buf) = config2(h, SERVICE_CONFIG_LAUNCH_PROTECTED) {
        // SAFETY: the call succeeded for this info level.
        let d = unsafe { &*(buf.as_ptr() as *const SERVICE_LAUNCH_PROTECTED_INFO) };
        s.protected = d.dwLaunchProtected != SERVICE_LAUNCH_PROTECTED_NONE;
    }
}

fn config2(h: &Handle, level: u32) -> Option<Vec<u64>> {
    query_buffer(|p, len, needed| {
        // SAFETY: `p`/`len` describe a writable buffer (or null/0 to size it).
        unsafe { QueryServiceConfig2W(h.0, level, p, len, needed) }
    })
}

fn last_error<T>(ok: i32, v: T) -> Result<T, u32> {
    if ok != 0 {
        Ok(v)
    } else {
        // SAFETY: plain thread-local error read.
        Err(unsafe { GetLastError() })
    }
}

fn query_state(h: &Handle) -> Result<u32, u32> {
    // SAFETY: SERVICE_STATUS is plain data; zeroed is a valid value.
    let mut st: SERVICE_STATUS = unsafe { std::mem::zeroed() };
    // SAFETY: `h` has SERVICE_QUERY_STATUS and `st` is a valid out-param.
    let ok = unsafe { QueryServiceStatus(h.0, &mut st) };
    last_error(ok, st.dwCurrentState)
}

fn wait_for(h: &Handle, want: u32, timeout: Duration) -> Result<(), u32> {
    let t0 = Instant::now();
    loop {
        if query_state(h)? == want {
            return Ok(());
        }
        if t0.elapsed() > timeout {
            return Err(ERROR_TIMEOUT);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Performs `action` on service `name` with the current token's rights.
/// Returns the Win32 error code on failure (ERROR_ACCESS_DENIED when the
/// caller should retry elevated).
pub fn perform(name: &str, action: Action) -> Result<(), u32> {
    let scm = open_manager(SC_MANAGER_CONNECT)?;
    let w = wide(name);
    let timeout = Duration::from_secs(30);
    match action {
        Action::Start => {
            let h = open_service(&scm, &w, SERVICE_START)?;
            // SAFETY: no arguments are passed to the service.
            last_error(unsafe { StartServiceW(h.0, 0, null()) }, ())
        }
        Action::Stop => stop(&scm, &w, timeout),
        Action::Restart => {
            match stop(&scm, &w, timeout) {
                Ok(()) | Err(ERROR_SERVICE_NOT_ACTIVE) => {}
                Err(e) => return Err(e),
            }
            let h = open_service(&scm, &w, SERVICE_START)?;
            // SAFETY: no arguments are passed to the service.
            last_error(unsafe { StartServiceW(h.0, 0, null()) }, ())
        }
        Action::SetStart(t) => {
            let (code, delayed) = match t {
                StartType::Auto => (SERVICE_AUTO_START, false),
                StartType::AutoDelayed => (SERVICE_AUTO_START, true),
                StartType::Manual => (SERVICE_DEMAND_START, false),
                StartType::Disabled => (SERVICE_DISABLED, false),
                _ => return Err(ERROR_ACCESS_DENIED),
            };
            let h = open_service(&scm, &w, SERVICE_CHANGE_CONFIG)?;
            // SAFETY: SERVICE_NO_CHANGE / null keep every other setting.
            let ok = unsafe {
                ChangeServiceConfigW(
                    h.0,
                    SERVICE_NO_CHANGE,
                    code,
                    SERVICE_NO_CHANGE,
                    null(),
                    null(),
                    null_mut(),
                    null(),
                    null(),
                    null(),
                    null(),
                )
            };
            last_error(ok, ())?;
            if code == SERVICE_AUTO_START {
                let mut info = SERVICE_DELAYED_AUTO_START_INFO {
                    fDelayedAutostart: delayed as i32,
                };
                // SAFETY: `info` matches the info level and outlives the call.
                let ok = unsafe {
                    ChangeServiceConfig2W(
                        h.0,
                        SERVICE_CONFIG_DELAYED_AUTO_START_INFO,
                        (&mut info as *mut SERVICE_DELAYED_AUTO_START_INFO).cast(),
                    )
                };
                last_error(ok, ())?;
            }
            Ok(())
        }
    }
}

fn stop(scm: &Handle, w: &[u16], timeout: Duration) -> Result<(), u32> {
    let h = open_service(scm, w, SERVICE_STOP | SERVICE_QUERY_STATUS)?;
    // SAFETY: SERVICE_STATUS is plain data; zeroed is a valid value.
    let mut st: SERVICE_STATUS = unsafe { std::mem::zeroed() };
    // SAFETY: `h` has SERVICE_STOP and `st` is a valid out-param.
    let ok = unsafe { ControlService(h.0, SERVICE_CONTROL_STOP, &mut st) };
    last_error(ok, ())?;
    wait_for(&h, SERVICE_STOPPED, timeout)
}
