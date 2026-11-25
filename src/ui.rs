//! The main window: search box, virtual list view, status bar, context menu.
//!
//! State lives in a thread-local `RefCell`. Messages sent to child controls
//! can call back into the window procedure synchronously, so no borrow is
//! ever held across a `SendMessageW`: handlers copy what they need out of
//! the state, release it, then talk to the controls.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
use windows_sys::Win32::System::Services::{
    SERVICE_CONTINUE_PENDING, SERVICE_PAUSE_PENDING, SERVICE_PAUSED, SERVICE_RUNNING,
    SERVICE_START_PENDING, SERVICE_STOP_PENDING, SERVICE_STOPPED,
};
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForWindow, GetSystemMetricsForDpi, SystemParametersInfoForDpi,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_DOWN, VK_F5};
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::i18n::{self, Strings};
use crate::scm::{self, Action, Service, StartType};
use crate::util::{self, wide};

const ID_SEARCH: usize = 100;
const ID_LIST: usize = 101;
const ID_STATUS: usize = 102;
const ID_ADMIN: usize = 103;

const IDM_START: usize = 200;
const IDM_STOP: usize = 201;
const IDM_RESTART: usize = 202;
const IDM_AUTO: usize = 210;
const IDM_DELAYED: usize = 211;
const IDM_MANUAL: usize = 212;
const IDM_DISABLED: usize = 213;
const IDM_OPEN_LOCATION: usize = 220;
const IDM_COPY_NAME: usize = 221;
const IDM_COPY_DISPLAY: usize = 222;
const IDM_REFRESH: usize = 230;
const IDM_FIND: usize = 231;

const WM_APP_LOADED: u32 = WM_APP + 1;
const WM_APP_DONE: u32 = WM_APP + 2;
const TIMER_STATES: usize = 1;

/// Column widths at 96 DPI.
const COLUMN_WIDTHS: [i32; 7] = [230, 150, 130, 170, 170, 320, 340];

const IMG_SHIELD: i32 = 0;
const IMG_LOCK: i32 = 1;

struct App {
    hwnd: HWND,
    search: HWND,
    list: HWND,
    status: HWND,
    admin: HWND,
    font: HFONT,
    font_px: i32,
    images: HIMAGELIST,
    shield_bmp: HBITMAP,
    s: &'static Strings,
    elevated: bool,
    dpi: u32,
    services: Vec<Service>,
    /// Lower-cased text of every column, one entry per service.
    hay: Vec<String>,
    /// Indices into `services`, filtered and sorted, as shown.
    view: Vec<usize>,
    query: Vec<String>,
    sort_col: usize,
    sort_asc: bool,
    busy: u32,
    loaded: bool,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Runs `f` on the state if it is available (not mid-borrow, not destroyed).
fn with<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

fn send(h: HWND, msg: u32, w: usize, l: isize) -> isize {
    // SAFETY: plain message send to a window we created.
    unsafe { SendMessageW(h, msg, w, l) }
}

fn scale(v: i32, dpi: u32) -> i32 {
    v * dpi as i32 / 96
}

fn state_text(s: &Strings, state: u32) -> &'static str {
    match state {
        SERVICE_RUNNING => s.running,
        SERVICE_STOPPED => s.stopped,
        SERVICE_START_PENDING => s.starting,
        SERVICE_STOP_PENDING => s.stopping,
        SERVICE_PAUSED => s.paused,
        SERVICE_PAUSE_PENDING => s.pausing,
        SERVICE_CONTINUE_PENDING => s.resuming,
        _ => "",
    }
}

fn start_text(s: &Strings, t: StartType) -> &'static str {
    match t {
        StartType::Boot => s.boot,
        StartType::System => s.system,
        StartType::Auto => s.automatic,
        StartType::AutoDelayed => s.automatic_delayed,
        StartType::Manual => s.manual,
        StartType::Disabled => s.disabled,
        StartType::Unknown => "",
    }
}

fn state_rank(state: u32) -> u8 {
    match state {
        SERVICE_RUNNING => 0,
        SERVICE_START_PENDING
        | SERVICE_STOP_PENDING
        | SERVICE_CONTINUE_PENDING
        | SERVICE_PAUSE_PENDING => 1,
        SERVICE_PAUSED => 2,
        _ => 3,
    }
}

impl App {
    fn cell<'a>(&self, sv: &'a Service, col: i32) -> &'a str {
        match col {
            0 => &sv.display,
            1 => &sv.name,
            2 => state_text(self.s, sv.state),
            3 => start_text(self.s, sv.start),
            4 => &sv.account,
            5 => &sv.description,
            _ => &sv.path,
        }
    }

    fn haystack(&self, sv: &Service) -> String {
        (0..7)
            .map(|c| self.cell(sv, c))
            .collect::<Vec<_>>()
            .join("\u{1}")
            .to_lowercase()
    }

    fn rebuild_hay(&mut self) {
        self.hay = self.services.iter().map(|sv| self.haystack(sv)).collect();
    }

    fn icon_for(&self, sv: &Service) -> i32 {
        if sv.protected || (self.elevated && !sv.can_control) {
            IMG_LOCK
        } else if !sv.can_control {
            IMG_SHIELD
        } else {
            I_IMAGENONE
        }
    }

    /// Recomputes `view` from the query and the sort order.
    fn refilter(&mut self) {
        let words = &self.query;
        let mut view: Vec<usize> = (0..self.services.len())
            .filter(|&i| {
                let h = &self.hay[i];
                words.iter().all(|w| match w.strip_prefix('-') {
                    Some(neg) if !neg.is_empty() => !h.contains(neg),
                    _ => h.contains(w.as_str()),
                })
            })
            .collect();
        let col = self.sort_col;
        let sv = &self.services;
        view.sort_by(|&a, &b| {
            let (x, y) = (&sv[a], &sv[b]);
            let o = match col {
                2 => state_rank(x.state).cmp(&state_rank(y.state)),
                3 => x.start.rank().cmp(&y.start.rank()),
                c => self
                    .cell(x, c as i32)
                    .to_lowercase()
                    .cmp(&self.cell(y, c as i32).to_lowercase()),
            };
            let o = if o == Ordering::Equal {
                x.display.to_lowercase().cmp(&y.display.to_lowercase())
            } else {
                o
            };
            if self.sort_asc { o } else { o.reverse() }
        });
        self.view = view;
    }

    fn count_text(&self) -> String {
        if !self.loaded {
            return self.s.working.to_string();
        }
        self.s
            .count
            .replace("{shown}", &self.view.len().to_string())
            .replace("{total}", &self.services.len().to_string())
    }

    fn status_right(&self) -> String {
        if self.busy > 0 {
            self.s.working.to_string()
        } else if self.elevated {
            self.s.elevated.to_string()
        } else {
            self.s.not_elevated.to_string()
        }
    }
}

/// Opens the main window and runs the message loop.
pub fn run() -> i32 {
    // SAFETY: Win32 window creation and message loop on the UI thread; all
    // pointers passed are to live locals or static wide strings.
    unsafe {
        let icc = INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES | ICC_BAR_CLASSES | ICC_STANDARD_CLASSES,
        };
        InitCommonControlsEx(&icc);
        let hinst = GetModuleHandleW(null());
        let class = wide("ServicesFinderMain");
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: LoadIconW(hinst, 1 as _),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_WINDOW + 1) as usize as HBRUSH,
            lpszMenuName: null(),
            lpszClassName: class.as_ptr(),
            hIconSm: null_mut(),
        };
        RegisterClassExW(&wc);
        let s = i18n::current();
        let title = wide(s.title);
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        if hwnd.is_null() {
            return 1;
        }
        // Size the first window from the monitor DPI.
        let dpi = GetDpiForWindow(hwnd);
        SetWindowPos(
            hwnd,
            null_mut(),
            0,
            0,
            scale(1200, dpi),
            scale(720, dpi),
            SWP_NOMOVE | SWP_NOZORDER,
        );
        ShowWindow(hwnd, SW_SHOWNORMAL);

        let accels = [
            ACCEL {
                fVirt: FVIRTKEY | FCONTROL,
                key: b'F' as u16,
                cmd: IDM_FIND as u16,
            },
            ACCEL {
                fVirt: FVIRTKEY,
                key: VK_F5,
                cmd: IDM_REFRESH as u16,
            },
        ];
        let haccel = CreateAcceleratorTableW(accels.as_ptr(), accels.len() as i32);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            if TranslateAcceleratorW(hwnd, haccel, &msg) != 0 {
                continue;
            }
            if IsDialogMessageW(hwnd, &msg) != 0 {
                continue;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        DestroyAcceleratorTable(haccel);
        msg.wParam as i32
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            create(hwnd);
            0
        }
        WM_SIZE => {
            layout();
            0
        }
        WM_GETMINMAXINFO => {
            // SAFETY: lParam points to the MINMAXINFO the system passes.
            let mmi = unsafe { &mut *(lp as *mut MINMAXINFO) };
            // SAFETY: plain query on our own window.
            let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
            mmi.ptMinTrackSize = POINT {
                x: scale(520, dpi),
                y: scale(300, dpi),
            };
            0
        }
        WM_DPICHANGED => {
            // SAFETY: lParam points to the suggested window RECT.
            let r = unsafe { *(lp as *const RECT) };
            dpi_changed((wp & 0xffff) as u32);
            // SAFETY: repositioning our own window to the system's suggestion.
            unsafe {
                SetWindowPos(
                    hwnd,
                    null_mut(),
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                )
            };
            0
        }
        WM_SETFOCUS => {
            if let Some(search) = with(|a| a.search) {
                // SAFETY: focusing our own child.
                unsafe { SetFocus(search) };
            }
            0
        }
        WM_COMMAND => {
            let id = wp & 0xffff;
            let code = ((wp >> 16) & 0xffff) as u32;
            if id == ID_SEARCH && code == EN_CHANGE {
                search_changed();
            } else if id == ID_ADMIN && code == BN_CLICKED {
                relaunch_elevated(hwnd);
            } else if id == IDCANCEL as usize {
                clear_search();
            } else {
                command(id);
            }
            0
        }
        WM_NOTIFY => notify(lp),
        WM_CONTEXTMENU => {
            let list = with(|a| a.list);
            if list == Some(wp as HWND) {
                context_menu(lp);
                0
            } else {
                // SAFETY: default handling.
                unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
            }
        }
        WM_TIMER if wp == TIMER_STATES => {
            refresh_states();
            0
        }
        WM_APP_LOADED => {
            // SAFETY: lParam is the Box leaked by the loader thread for us.
            let r = unsafe { Box::from_raw(lp as *mut Result<Vec<Service>, u32>) };
            loaded(*r);
            0
        }
        WM_APP_DONE => {
            action_done(wp as u32);
            0
        }
        WM_DESTROY => {
            if let Some(a) = APP.with(|a| a.borrow_mut().take()) {
                // SAFETY: GDI objects and the image list we created.
                unsafe {
                    DeleteObject(a.font);
                    DeleteObject(a.shield_bmp);
                    ImageList_Destroy(a.images);
                    KillTimer(hwnd, TIMER_STATES);
                    PostQuitMessage(0);
                }
            }
            0
        }
        // SAFETY: default handling for everything else.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn child(parent: HWND, class: *const u16, text: &str, style: u32, ex: u32, id: usize) -> HWND {
    let t = wide(text);
    // SAFETY: creates a child control of our window; strings outlive the call.
    unsafe {
        CreateWindowExW(
            ex,
            class,
            t.as_ptr(),
            WS_CHILD | WS_VISIBLE | style,
            0,
            0,
            0,
            0,
            parent,
            id as HMENU,
            GetModuleHandleW(null()),
            null(),
        )
    }
}

fn create(hwnd: HWND) {
    let s = i18n::current();
    let elevated = util::is_elevated();
    // SAFETY: plain query on our own window.
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    let search = child(
        hwnd,
        WC_EDITW,
        "",
        WS_TABSTOP | ES_AUTOHSCROLL as u32,
        WS_EX_CLIENTEDGE,
        ID_SEARCH,
    );
    let cue = wide(s.search_cue);
    send(search, EM_SETCUEBANNER, 1, cue.as_ptr() as isize);
    // SAFETY: subclassing our own edit control with a static procedure.
    unsafe { SetWindowSubclass(search, Some(search_proc), 1, 0) };

    let admin = child(
        hwnd,
        WC_BUTTONW,
        s.run_as_admin,
        WS_TABSTOP | BS_PUSHBUTTON as u32,
        0,
        ID_ADMIN,
    );
    send(admin, BCM_SETSHIELD, 0, 1);
    if elevated {
        // SAFETY: hiding our own child.
        unsafe { ShowWindow(admin, SW_HIDE) };
    }

    let list = child(
        hwnd,
        WC_LISTVIEWW,
        "",
        WS_TABSTOP | (LVS_REPORT | LVS_OWNERDATA | LVS_SINGLESEL | LVS_SHOWSELALWAYS),
        0,
        ID_LIST,
    );
    let ex = LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_INFOTIP | LVS_EX_HEADERDRAGDROP;
    send(list, LVM_SETEXTENDEDLISTVIEWSTYLE, ex as usize, ex as isize);
    let theme = wide("Explorer");
    // SAFETY: applies the Explorer visual style to our list view.
    unsafe { SetWindowTheme(list, theme.as_ptr(), null()) };
    for (i, name) in s.columns.iter().enumerate() {
        let mut text = wide(name);
        let col = LVCOLUMNW {
            mask: LVCF_TEXT | LVCF_WIDTH | LVCF_SUBITEM,
            fmt: LVCFMT_LEFT,
            cx: scale(COLUMN_WIDTHS[i], dpi),
            pszText: text.as_mut_ptr(),
            cchTextMax: 0,
            iSubItem: i as i32,
            iImage: 0,
            iOrder: 0,
            cxMin: 0,
            cxDefault: 0,
            cxIdeal: 0,
        };
        send(
            list,
            LVM_INSERTCOLUMNW,
            i,
            &col as *const LVCOLUMNW as isize,
        );
    }

    let status = child(hwnd, STATUSCLASSNAMEW, "", SBARS_SIZEGRIP, 0, ID_STATUS);

    let (images, shield_bmp) = make_images(dpi);
    send(list, LVM_SETIMAGELIST, LVSIL_SMALL as usize, images);
    let (font, font_px) = make_font(dpi);
    for h in [search, admin, list, status] {
        send(h, WM_SETFONT, font as usize, 1);
    }

    APP.with(|a| {
        *a.borrow_mut() = Some(App {
            hwnd,
            search,
            list,
            status,
            admin,
            font,
            font_px,
            images,
            shield_bmp,
            s,
            elevated,
            dpi,
            services: Vec::new(),
            hay: Vec::new(),
            view: Vec::new(),
            query: Vec::new(),
            sort_col: 0,
            sort_asc: true,
            busy: 0,
            loaded: false,
        })
    });
    set_sort_arrow();
    update_status();
    start_load();
    // SAFETY: timer on our own window.
    unsafe { SetTimer(hwnd, TIMER_STATES, 2000, None) };
}

fn make_font(dpi: u32) -> (HFONT, i32) {
    // SAFETY: NONCLIENTMETRICSW is plain data; the system fills it.
    unsafe {
        let mut ncm: NONCLIENTMETRICSW = std::mem::zeroed();
        ncm.cbSize = size_of::<NONCLIENTMETRICSW>() as u32;
        SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS,
            ncm.cbSize,
            (&mut ncm as *mut NONCLIENTMETRICSW).cast(),
            0,
            dpi,
        );
        let px = ncm.lfMessageFont.lfHeight.abs().max(scale(12, dpi));
        (CreateFontIndirectW(&ncm.lfMessageFont), px)
    }
}

/// System stock icon (shield, padlock) at `cx` pixels.
fn stock_icon(id: SHSTOCKICONID, cx: i32) -> HICON {
    // SAFETY: SHSTOCKICONINFO is plain data with its size set; the icon file
    // path is NUL-terminated by the system.
    unsafe {
        let mut sii: SHSTOCKICONINFO = std::mem::zeroed();
        sii.cbSize = size_of::<SHSTOCKICONINFO>() as u32;
        if SHGetStockIconInfo(id, SHGSI_ICONLOCATION, &mut sii) != 0 {
            return null_mut();
        }
        let mut icon: HICON = null_mut();
        let size = cx as u32 & 0xffff;
        SHDefExtractIconW(
            sii.szPath.as_ptr(),
            sii.iIcon,
            0,
            &mut icon,
            null_mut(),
            size,
        );
        icon
    }
}

/// 32-bit premultiplied bitmap of an icon, for menu items.
fn icon_bitmap(icon: HICON, cx: i32) -> HBITMAP {
    // SAFETY: standard DIB section drawing; every GDI object is released.
    unsafe {
        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = cx;
        bi.bmiHeader.biHeight = -cx;
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB;
        let dc = CreateCompatibleDC(null_mut());
        let mut bits = null_mut();
        let bmp = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if !bmp.is_null() {
            let old = SelectObject(dc, bmp);
            DrawIconEx(dc, 0, 0, icon, cx, cx, 0, null_mut(), DI_NORMAL);
            SelectObject(dc, old);
        }
        DeleteDC(dc);
        bmp
    }
}

fn make_images(dpi: u32) -> (HIMAGELIST, HBITMAP) {
    // SAFETY: image list and icons are created here and owned by the caller.
    unsafe {
        let cx = GetSystemMetricsForDpi(SM_CXSMICON, dpi);
        let il = ImageList_Create(cx, cx, ILC_COLOR32 | ILC_MASK, 2, 0);
        let shield = stock_icon(SIID_SHIELD, cx);
        let lock = stock_icon(SIID_LOCK, cx);
        ImageList_ReplaceIcon(il, -1, shield);
        ImageList_ReplaceIcon(il, -1, lock);
        let bmp = icon_bitmap(shield, cx);
        DestroyIcon(shield);
        DestroyIcon(lock);
        (il, bmp)
    }
}

fn dpi_changed(dpi: u32) {
    let Some((old_dpi, list, controls, old_font, old_images, old_bmp)) = with(|a| {
        let r = (
            a.dpi,
            a.list,
            [a.search, a.admin, a.list, a.status],
            a.font,
            a.images,
            a.shield_bmp,
        );
        a.dpi = dpi;
        r
    }) else {
        return;
    };
    let (font, px) = make_font(dpi);
    let (images, bmp) = make_images(dpi);
    with(|a| {
        a.font = font;
        a.font_px = px;
        a.images = images;
        a.shield_bmp = bmp;
    });
    for h in controls {
        send(h, WM_SETFONT, font as usize, 1);
    }
    send(list, LVM_SETIMAGELIST, LVSIL_SMALL as usize, images);
    for i in 0..COLUMN_WIDTHS.len() {
        let w = send(list, LVM_GETCOLUMNWIDTH, i, 0) as i32;
        send(
            list,
            LVM_SETCOLUMNWIDTH,
            i,
            (w * dpi as i32 / old_dpi.max(1) as i32) as isize,
        );
    }
    // SAFETY: the old objects are no longer selected anywhere.
    unsafe {
        DeleteObject(old_font);
        DeleteObject(old_bmp);
        ImageList_Destroy(old_images);
    }
}

fn layout() {
    let Some((hwnd, search, admin, list, status, dpi, px, elevated)) = with(|a| {
        (
            a.hwnd, a.search, a.admin, a.list, a.status, a.dpi, a.font_px, a.elevated,
        )
    }) else {
        return;
    };
    // SAFETY: geometry calls on our own windows with valid RECT out-params.
    unsafe {
        let mut rc: RECT = std::mem::zeroed();
        GetClientRect(hwnd, &mut rc);
        send(status, WM_SIZE, 0, 0);
        let mut sr: RECT = std::mem::zeroed();
        GetWindowRect(status, &mut sr);
        let sh = sr.bottom - sr.top;
        let w = rc.right;
        let m = scale(8, dpi);
        let eh = px + scale(12, dpi);
        let aw = if elevated { 0 } else { scale(250, dpi) };
        let sw = w - 2 * m - if aw > 0 { aw + m } else { 0 };
        MoveWindow(search, m, m, sw.max(50), eh, 1);
        if aw > 0 {
            MoveWindow(admin, w - m - aw, m, aw, eh, 1);
        }
        let top = m + eh + m;
        MoveWindow(list, 0, top, w, (rc.bottom - top - sh).max(0), 1);
        let parts = [w * 2 / 5, -1];
        send(status, SB_SETPARTS, 2, parts.as_ptr() as isize);
    }
    update_status();
}

fn set_status_part(status: HWND, part: usize, text: &str) {
    let t = wide(text);
    send(status, SB_SETTEXTW, part, t.as_ptr() as isize);
}

fn update_status() {
    if let Some((status, left, right)) = with(|a| (a.status, a.count_text(), a.status_right())) {
        set_status_part(status, 0, &left);
        set_status_part(status, 1, &right);
    }
}

fn start_load() {
    let Some(hwnd) = with(|a| {
        a.busy += 1;
        a.hwnd as isize
    }) else {
        return;
    };
    update_status();
    std::thread::spawn(move || {
        let r = Box::new(scm::enumerate());
        // SAFETY: the window procedure takes ownership back with Box::from_raw.
        unsafe { PostMessageW(hwnd as HWND, WM_APP_LOADED, 0, Box::into_raw(r) as isize) };
    });
}

fn loaded(r: Result<Vec<Service>, u32>) {
    let keep = selected().map(|sv| sv.name);
    let err = with(|a| {
        a.busy = a.busy.saturating_sub(1);
        match r {
            Ok(v) => {
                a.services = v;
                a.loaded = true;
                a.rebuild_hay();
                None
            }
            Err(e) => Some((a.hwnd, a.s.action_failed, e)),
        }
    })
    .flatten();
    if let Some((hwnd, title, e)) = err {
        message(hwnd, title, &util::error_text(e));
    }
    update_list(keep);
}

/// Pushes the filtered view to the list control, keeping `keep` selected.
fn update_list(keep: Option<String>) {
    let Some((list, n, pos)) = with(|a| {
        a.refilter();
        let pos = keep.and_then(|k| a.view.iter().position(|&i| a.services[i].name == k));
        (a.list, a.view.len(), pos)
    }) else {
        return;
    };
    send(
        list,
        LVM_SETITEMCOUNT,
        n,
        (LVSICF_NOSCROLL | LVSICF_NOINVALIDATEALL) as isize,
    );
    // SAFETY: repaint request for our own list.
    unsafe { InvalidateRect(list, null(), 0) };
    if let Some(p) = pos {
        select(list, p);
    }
    update_status();
}

fn select(list: HWND, index: usize) {
    let mut item: LVITEMW = unsafe { std::mem::zeroed() };
    item.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
    item.state = LVIS_SELECTED | LVIS_FOCUSED;
    send(
        list,
        LVM_SETITEMSTATE,
        index,
        &item as *const LVITEMW as isize,
    );
    send(list, LVM_ENSUREVISIBLE, index, 0);
}

fn selected_index() -> Option<usize> {
    let list = with(|a| a.list)?;
    let i = send(list, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize);
    usize::try_from(i).ok()
}

fn selected() -> Option<Service> {
    let i = selected_index()?;
    with(|a| a.view.get(i).map(|&k| a.services[k].clone())).flatten()
}

fn window_text(h: HWND) -> String {
    // SAFETY: the buffer is sized from the reported length plus the NUL.
    unsafe {
        let n = GetWindowTextLengthW(h);
        let mut buf = vec![0u16; n as usize + 1];
        let got = GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32);
        String::from_utf16_lossy(&buf[..got as usize])
    }
}

fn search_changed() {
    let Some(search) = with(|a| a.search) else {
        return;
    };
    let text = window_text(search).to_lowercase();
    let keep = selected().map(|sv| sv.name);
    with(|a| a.query = text.split_whitespace().map(str::to_string).collect());
    update_list(keep);
}

fn clear_search() {
    let Some(search) = with(|a| a.search) else {
        return;
    };
    let empty = [0u16];
    // SAFETY: setting text and focus on our own edit control.
    unsafe {
        SetWindowTextW(search, empty.as_ptr());
        SetFocus(search);
    }
}

unsafe extern "system" fn search_proc(
    h: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if msg == WM_KEYDOWN && wp == VK_DOWN as usize {
        if let Some((list, n)) = with(|a| (a.list, a.view.len())) {
            if n > 0 {
                let i = selected_index().unwrap_or(0);
                // SAFETY: focusing our own list.
                unsafe { SetFocus(list) };
                select(list, i);
            }
        }
        return 0;
    }
    // SAFETY: default subclass chain.
    unsafe { DefSubclassProc(h, msg, wp, lp) }
}

fn set_sort_arrow() {
    let Some((list, col, asc)) = with(|a| (a.list, a.sort_col, a.sort_asc)) else {
        return;
    };
    let header = send(list, LVM_GETHEADER, 0, 0) as HWND;
    for i in 0..COLUMN_WIDTHS.len() {
        let mut item: HDITEMW = unsafe { std::mem::zeroed() };
        item.mask = HDI_FORMAT;
        send(header, HDM_GETITEMW, i, &mut item as *mut HDITEMW as isize);
        item.fmt &= !(HDF_SORTUP | HDF_SORTDOWN);
        if i == col {
            item.fmt |= if asc { HDF_SORTUP } else { HDF_SORTDOWN };
        }
        send(header, HDM_SETITEMW, i, &item as *const HDITEMW as isize);
    }
}

fn copy_into(dst: *mut u16, cap: i32, text: &str) {
    if dst.is_null() || cap <= 0 {
        return;
    }
    let mut n = 0usize;
    for (i, u) in text.encode_utf16().take(cap as usize - 1).enumerate() {
        // SAFETY: `i` < cap - 1 and the control gave us `cap` writable units.
        unsafe { *dst.add(i) = u };
        n = i + 1;
    }
    // SAFETY: n <= cap - 1.
    unsafe { *dst.add(n) = 0 };
}

fn notify(lp: LPARAM) -> LRESULT {
    // SAFETY: WM_NOTIFY's lParam always starts with an NMHDR.
    let hdr = unsafe { &*(lp as *const NMHDR) };
    if hdr.idFrom != ID_LIST {
        return 0;
    }
    match hdr.code {
        LVN_GETDISPINFOW => {
            // SAFETY: this notification carries an NMLVDISPINFOW.
            let di = unsafe { &mut *(lp as *mut NMLVDISPINFOW) };
            with(|a| {
                let Some(&k) = a.view.get(di.item.iItem as usize) else {
                    return;
                };
                let sv = &a.services[k];
                if di.item.mask & LVIF_TEXT != 0 {
                    copy_into(
                        di.item.pszText,
                        di.item.cchTextMax,
                        a.cell(sv, di.item.iSubItem),
                    );
                }
                if di.item.mask & LVIF_IMAGE != 0 {
                    di.item.iImage = a.icon_for(sv);
                }
            });
        }
        LVN_GETINFOTIPW => {
            // SAFETY: this notification carries an NMLVGETINFOTIPW.
            let tip = unsafe { &mut *(lp as *mut NMLVGETINFOTIPW) };
            with(|a| {
                let Some(&k) = a.view.get(tip.iItem as usize) else {
                    return;
                };
                let sv = &a.services[k];
                let text = match a.icon_for(sv) {
                    IMG_LOCK => format!("{}\n{}", a.s.protected_tip, sv.description),
                    IMG_SHIELD => format!("{}\n{}", a.s.shield_tip, sv.description),
                    _ => sv.description.clone(),
                };
                copy_into(tip.pszText, tip.cchTextMax, text.trim());
            });
        }
        LVN_COLUMNCLICK => {
            // SAFETY: this notification carries an NMLISTVIEW.
            let nm = unsafe { &*(lp as *const NMLISTVIEW) };
            let col = nm.iSubItem as usize;
            let keep = selected().map(|sv| sv.name);
            with(|a| {
                if a.sort_col == col {
                    a.sort_asc = !a.sort_asc;
                } else {
                    a.sort_col = col;
                    a.sort_asc = true;
                }
            });
            set_sort_arrow();
            update_list(keep);
        }
        _ => {}
    }
    0
}

fn context_menu(lp: LPARAM) {
    let Some(sv) = selected() else {
        return;
    };
    let Some((hwnd, list, s, elevated, bmp)) =
        with(|a| (a.hwnd, a.list, a.s, a.elevated, a.shield_bmp))
    else {
        return;
    };
    let (mut x, mut y) = (
        (lp & 0xffff) as i16 as i32,
        ((lp >> 16) & 0xffff) as i16 as i32,
    );
    if lp == -1 {
        // Keyboard: open at the selected row.
        if let Some(i) = selected_index() {
            let mut r = RECT {
                left: LVIR_LABEL as i32,
                top: 0,
                right: 0,
                bottom: 0,
            };
            send(list, LVM_GETITEMRECT, i, &mut r as *mut RECT as isize);
            let mut p = POINT {
                x: r.left,
                y: r.bottom,
            };
            // SAFETY: converting a point of our own list.
            unsafe { ClientToScreen(list, &mut p) };
            (x, y) = (p.x, p.y);
        }
    }
    let running = sv.state == SERVICE_RUNNING;
    let stopped = sv.state == SERVICE_STOPPED;
    let control_shield = !elevated && !sv.can_control && !sv.protected;
    let config_shield = !elevated && !sv.can_config;
    // SAFETY: menu construction and tracking; the menu is destroyed after use.
    let cmd = unsafe {
        let m = CreatePopupMenu();
        let add = |menu: HMENU, id: usize, text: &str, enabled: bool, shield: bool| {
            let t = wide(text);
            AppendMenuW(
                menu,
                MF_STRING | if enabled { 0 } else { MF_GRAYED },
                id,
                t.as_ptr(),
            );
            if shield && !bmp.is_null() {
                let mut mii: MENUITEMINFOW = std::mem::zeroed();
                mii.cbSize = size_of::<MENUITEMINFOW>() as u32;
                mii.fMask = MIIM_BITMAP;
                mii.hbmpItem = bmp;
                SetMenuItemInfoW(menu, id as u32, 0, &mii);
            }
        };
        let sep = |menu: HMENU| {
            AppendMenuW(menu, MF_SEPARATOR, 0, null());
        };
        add(m, IDM_START, s.start, stopped, control_shield);
        add(
            m,
            IDM_STOP,
            s.stop,
            running && !sv.protected,
            control_shield,
        );
        add(
            m,
            IDM_RESTART,
            s.restart,
            running && !sv.protected,
            control_shield,
        );
        let sub = CreatePopupMenu();
        let configurable = matches!(
            sv.start,
            StartType::Auto | StartType::AutoDelayed | StartType::Manual | StartType::Disabled
        ) && !sv.protected;
        add(sub, IDM_AUTO, s.automatic, configurable, config_shield);
        add(
            sub,
            IDM_DELAYED,
            s.automatic_delayed,
            configurable,
            config_shield,
        );
        add(sub, IDM_MANUAL, s.manual, configurable, config_shield);
        add(sub, IDM_DISABLED, s.disabled, configurable, config_shield);
        let current = match sv.start {
            StartType::Auto => Some(IDM_AUTO),
            StartType::AutoDelayed => Some(IDM_DELAYED),
            StartType::Manual => Some(IDM_MANUAL),
            StartType::Disabled => Some(IDM_DISABLED),
            _ => None,
        };
        if let Some(c) = current {
            CheckMenuRadioItem(
                sub,
                IDM_AUTO as u32,
                IDM_DISABLED as u32,
                c as u32,
                MF_BYCOMMAND,
            );
        }
        let st = wide(s.startup_type);
        AppendMenuW(m, MF_POPUP, sub as usize, st.as_ptr());
        sep(m);
        add(
            m,
            IDM_OPEN_LOCATION,
            s.open_location,
            !sv.path.is_empty(),
            false,
        );
        add(m, IDM_COPY_NAME, s.copy_name, true, false);
        add(m, IDM_COPY_DISPLAY, s.copy_display, true, false);
        sep(m);
        add(m, IDM_REFRESH, s.refresh, true, false);
        let cmd = TrackPopupMenu(m, TPM_RIGHTBUTTON | TPM_RETURNCMD, x, y, 0, hwnd, null());
        DestroyMenu(m);
        cmd
    };
    if cmd > 0 {
        command(cmd as usize);
    }
}

fn command(id: usize) {
    match id {
        IDM_FIND => {
            if let Some(search) = with(|a| a.search) {
                // SAFETY: focusing and selecting text in our own edit.
                unsafe { SetFocus(search) };
                send(search, EM_SETSEL, 0, -1);
            }
        }
        IDM_REFRESH => start_load(),
        IDM_START => act(Action::Start),
        IDM_STOP => act(Action::Stop),
        IDM_RESTART => act(Action::Restart),
        IDM_AUTO => act(Action::SetStart(StartType::Auto)),
        IDM_DELAYED => act(Action::SetStart(StartType::AutoDelayed)),
        IDM_MANUAL => act(Action::SetStart(StartType::Manual)),
        IDM_DISABLED => act(Action::SetStart(StartType::Disabled)),
        IDM_OPEN_LOCATION => {
            if let Some(sv) = selected() {
                open_location(&util::binary_from_command(&sv.path));
            }
        }
        IDM_COPY_NAME | IDM_COPY_DISPLAY => {
            if let (Some(sv), Some(hwnd)) = (selected(), with(|a| a.hwnd)) {
                let text = if id == IDM_COPY_NAME {
                    sv.name
                } else {
                    sv.display
                };
                util::set_clipboard(hwnd, &text);
            }
        }
        _ => {}
    }
}

fn open_location(path: &str) {
    let params = wide(&format!("/select,\"{path}\""));
    let exe = wide("explorer.exe");
    // SAFETY: launches Explorer with NUL-terminated arguments.
    unsafe {
        ShellExecuteW(
            null_mut(),
            null(),
            exe.as_ptr(),
            params.as_ptr(),
            null(),
            SW_SHOWNORMAL,
        );
    }
}

fn act(action: Action) {
    let Some(sv) = selected() else {
        return;
    };
    let Some((hwnd, elevated)) = with(|a| {
        a.busy += 1;
        (a.hwnd as isize, a.elevated)
    }) else {
        return;
    };
    update_status();
    let allowed = if action.is_control() {
        sv.can_control
    } else {
        sv.can_config
    };
    let name = sv.name;
    std::thread::spawn(move || {
        let r = if allowed || elevated {
            scm::perform(&name, action)
        } else {
            Err(ERROR_ACCESS_DENIED)
        };
        let r = match r {
            Err(ERROR_ACCESS_DENIED) if !elevated => elevated_helper(hwnd as HWND, &name, action),
            r => r,
        };
        let code = r.err().unwrap_or(0);
        // SAFETY: posting a result code to our window.
        unsafe { PostMessageW(hwnd as HWND, WM_APP_DONE, code as usize, 0) };
    });
}

fn exe_path() -> Vec<u16> {
    let mut buf = vec![0u16; 32768];
    // SAFETY: the buffer length matches; null module = this executable.
    let n = unsafe { GetModuleFileNameW(null_mut(), buf.as_mut_ptr(), buf.len() as u32) };
    buf.truncate(n as usize);
    buf.push(0);
    buf
}

/// Starts this executable elevated (UAC prompt). Returns its process handle
/// when `wait` is set.
fn shell_runas(hwnd: HWND, params: &str, wait: bool) -> Result<HANDLE, u32> {
    let exe = exe_path();
    let verb = wide("runas");
    let p = wide(params);
    // SAFETY: SHELLEXECUTEINFOW is plain data; strings outlive the call. COM
    // is initialised on this thread as ShellExecuteEx requires.
    unsafe {
        CoInitializeEx(
            null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        );
        let mut sei: SHELLEXECUTEINFOW = std::mem::zeroed();
        sei.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
        sei.fMask = if wait { SEE_MASK_NOCLOSEPROCESS } else { 0 };
        sei.hwnd = hwnd;
        sei.lpVerb = verb.as_ptr();
        sei.lpFile = exe.as_ptr();
        sei.lpParameters = p.as_ptr();
        sei.nShow = if wait { SW_HIDE } else { SW_SHOWNORMAL };
        if ShellExecuteExW(&mut sei) == 0 {
            return Err(GetLastError());
        }
        Ok(sei.hProcess)
    }
}

/// Runs one action in an elevated copy of this program and returns its result.
fn elevated_helper(hwnd: HWND, name: &str, action: Action) -> Result<(), u32> {
    let h = shell_runas(
        hwnd,
        &format!("--do {} \"{}\"", action.as_arg(), name),
        true,
    )?;
    if h.is_null() {
        return Ok(());
    }
    let mut code = 0u32;
    // SAFETY: `h` is the helper's process handle, closed after use.
    unsafe {
        WaitForSingleObject(h, INFINITE);
        GetExitCodeProcess(h, &mut code);
        CloseHandle(h);
    }
    if code == 0 { Ok(()) } else { Err(code) }
}

fn relaunch_elevated(hwnd: HWND) {
    if shell_runas(hwnd, "", false).is_ok() {
        // SAFETY: closing our own window; the elevated copy takes over.
        unsafe { DestroyWindow(hwnd) };
    }
}

fn action_done(code: u32) {
    let Some((hwnd, title)) = with(|a| {
        a.busy = a.busy.saturating_sub(1);
        (a.hwnd, a.s.action_failed)
    }) else {
        return;
    };
    if code != 0 && code != ERROR_CANCELLED {
        message(hwnd, title, &util::error_text(code));
    }
    start_load();
}

fn message(hwnd: HWND, title: &str, text: &str) {
    let t = wide(title);
    let m = wide(text);
    // SAFETY: modal message box owned by our window.
    unsafe { MessageBoxW(hwnd, m.as_ptr(), t.as_ptr(), MB_OK | MB_ICONWARNING) };
}

/// Cheap periodic update of the running states.
fn refresh_states() {
    let Some(true) = with(|a| a.loaded && a.busy == 0) else {
        return;
    };
    let Ok(states) = scm::enumerate_states() else {
        return;
    };
    let keep = selected().map(|sv| sv.name);
    let Some((changed, reload, list)) = with(|a| {
        if states.len() != a.services.len() {
            return (false, true, a.list);
        }
        let mut changed = false;
        for (i, (name, _, state, pid)) in states.iter().enumerate() {
            let Some(sv) = a.services.get(i) else {
                return (false, true, a.list);
            };
            if &sv.name != name {
                return (false, true, a.list);
            }
            if sv.state != *state || sv.pid != *pid {
                a.services[i].state = *state;
                a.services[i].pid = *pid;
                a.hay[i] = a.haystack(&a.services[i]);
                changed = true;
            }
        }
        (changed, false, a.list)
    }) else {
        return;
    };
    if reload {
        start_load();
    } else if changed {
        update_list(keep);
        // SAFETY: repaint request for our own list.
        unsafe { InvalidateRect(list, null(), 0) };
    }
}
