//! DevDeck — a native, ultra-light Windows Git client modeled on the VS Code
//! Source Control experience. Pure Win32 + GDI: no runtime, no web view.

#![windows_subsystem = "windows"]
#![allow(dead_code)]

mod app;
mod branches;
mod cli;
mod diff;
mod editor;
mod explorer;
mod git;
mod graph;
mod settings;
mod tabs;
mod term;
mod term_ui;
mod theme;
mod ui;
mod watch;
#[cfg(test)]
mod tests;
mod win;
#[cfg(feature = "guard-alloc")]
mod guardalloc;
#[cfg(feature = "guard-alloc")]
#[global_allocator]
static GLOBAL: guardalloc::GuardAlloc = guardalloc::GuardAlloc;

use std::cell::Cell;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use app::App;

const DEFERRED_EDIT: u32 = WM_APP + 3;
const WM_MOUSELEAVE: u32 = 0x02A3;
const WM_NCMOUSELEAVE: u32 = 0x02A2;

thread_local! {
    static BUSY: Cell<bool> = const { Cell::new(false) };
    static PENDING: Cell<u32> = const { Cell::new(0) };
    /// The file change timer is running: further changes don't restart it, so a file written
    /// more often than the delay (a log, a build cache) can't hold the refresh back forever.
    static FS_ARMED: Cell<bool> = const { Cell::new(false) };
}

/// A file changed: refresh shortly, at most one delay after the first change.
fn arm_fs_timer(hwnd: HWND) {
    if !FS_ARMED.with(|f| f.replace(true)) {
        unsafe {
            SetTimer(hwnd, app::TIMER_FS, 300, None);
        }
    }
}
const P_PAINT: u32 = 1;
const P_JOB: u32 = 2;
const P_FS: u32 = 4;
const P_TERM: u32 = 8;
/// WM_CLOSE while busy: closing then would destroy the window under the app (and skip the
/// unsaved files prompt), so it is posted again afterwards.
const P_CLOSE: u32 = 16;
/// WM_SIZE / WM_DPICHANGED while busy: layout (and fonts, after a DPI change) are redone afterwards.
const P_LAYOUT: u32 = 32;

fn app_ptr(hwnd: HWND) -> *mut App {
    unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App }
}

fn lo(l: LPARAM) -> i32 {
    (l & 0xFFFF) as i16 as i32
}

fn hi(l: LPARAM) -> i32 {
    ((l >> 16) & 0xFFFF) as i16 as i32
}

fn key_down(vk: u16) -> bool {
    unsafe { (GetKeyState(vk as i32) as u16 & 0x8000) != 0 }
}

/// Runs `f` with exclusive access to the App; messages arriving re-entrantly
/// (e.g. while a popup menu's modal loop runs) are deferred.
fn with_app<R>(hwnd: HWND, f: impl FnOnce(&mut App) -> R) -> Option<R> {
    let p = app_ptr(hwnd);
    if p.is_null() || BUSY.with(|b| b.get()) {
        return None;
    }
    BUSY.with(|b| b.set(true));
    let r = f(unsafe { &mut *p });
    BUSY.with(|b| b.set(false));
    // process what was deferred while busy
    loop {
        let pend = PENDING.with(|p| p.replace(0));
        if pend == 0 {
            break;
        }
        BUSY.with(|b| b.set(true));
        let app = unsafe { &mut *p };
        if pend & P_JOB != 0 {
            app.on_jobs();
        }
        if pend & P_TERM != 0 {
            app.on_term_data();
        }
        if pend & P_FS != 0 {
            arm_fs_timer(hwnd);
        }
        if pend & P_LAYOUT != 0 {
            let dpi = unsafe { GetDpiForWindow(hwnd) };
            if dpi != 0 && dpi != app.dpi {
                apply_dpi(app, dpi);
            }
            if unsafe { IsIconic(hwnd) } != 0 {
                app.release_buffer();
            } else {
                app.layout();
            }
        }
        if pend & P_PAINT != 0 {
            app.invalidate();
        }
        if pend & P_CLOSE != 0 {
            unsafe {
                PostMessageW(hwnd, WM_CLOSE, 0, 0);
            }
        }
        BUSY.with(|b| b.set(false));
    }
    Some(r)
}

// ------------------------------------------------------------ custom title bar
//
// The client area covers the native caption, so the title bar row (app icon, editor tabs,
// caption buttons) is painted by the app. The side and bottom frame stay native (resizing,
// shadow, Windows 11 rounded corners). The top resize edge, the caption buttons (HTMAXBUTTON
// keeps the Windows 11 snap layouts flyout) and dragging are restored with WM_NCHITTEST.

/// Thickness of the resize frame, which is also how far a maximized window hangs off screen.
unsafe fn frame_thickness(hwnd: HWND) -> i32 {
    let dpi = GetDpiForWindow(hwnd);
    GetSystemMetricsForDpi(SM_CYFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
}

/// WM_NCCALCSIZE: the native frame on the left, right and bottom, none on top. `lp` points
/// to an NCCALCSIZE_PARAMS when `wp` is TRUE, else (at creation) to a RECT; both start with
/// the rectangle to adjust.
unsafe fn nc_calc_size(hwnd: HWND, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // DefWindowProcW writes the rectangle through `lp`, so no reference to it may live across
    // that call (the optimizer would drop restoring `top` as a no-op store)
    let rc = if wp != 0 { std::ptr::addr_of_mut!((*(lp as *mut NCCALCSIZE_PARAMS)).rgrc[0]) } else { lp as *mut RECT };
    let top = std::ptr::read(std::ptr::addr_of!((*rc).top));
    let r = DefWindowProcW(hwnd, WM_NCCALCSIZE, wp, lp);
    let mut new_top = top;
    if IsZoomed(hwnd) != 0 {
        // the top edge of a maximized window is off screen by the frame's thickness
        new_top += frame_thickness(hwnd);
    }
    std::ptr::write(std::ptr::addr_of_mut!((*rc).top), new_top);
    r
}

/// WM_NCHITTEST: the native frame, then the top resize edge, then the title bar (`app` is
/// None while a message is handled re-entrantly; then only the caption buttons are known).
unsafe fn nc_hit_test(hwnd: HWND, lp: LPARAM, nested: bool) -> LRESULT {
    let r = DefWindowProcW(hwnd, WM_NCHITTEST, 0, lp);
    if r != HTCLIENT as LRESULT {
        return r;
    }
    let mut p = POINT { x: lo(lp), y: hi(lp) };
    ScreenToClient(hwnd, &mut p);
    let mut c: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut c);
    let frame = frame_thickness(hwnd);
    if IsZoomed(hwnd) == 0 && p.y < frame {
        return if p.x < frame * 2 {
            HTTOPLEFT
        } else if p.x >= c.right - frame * 2 {
            HTTOPRIGHT
        } else {
            HTTOP
        } as LRESULT;
    }
    if !nested {
        if let Some(h) = with_app(hwnd, |a| a.caption_hit(p.x, p.y)) {
            return h as LRESULT;
        }
    }
    // fallback from the geometry alone: the caption buttons are the three rightmost cells
    let scale = GetDpiForWindow(hwnd) as f32 / 96.0;
    let (th, cw) = ((app::TITLE_H as f32 * scale).round() as i32, (46.0 * scale).round() as i32);
    if p.y < th && p.x >= c.right - cw * 3 {
        return [HTMINBUTTON, HTMAXBUTTON, HTCLOSE][((p.x - (c.right - cw * 3)) / cw).clamp(0, 2) as usize] as LRESULT;
    }
    HTCLIENT as LRESULT
}

fn caption_button_hit(ht: usize) -> Option<app::Hit> {
    match ht as u32 {
        HTMINBUTTON => Some(app::Hit::CaptionMin),
        HTMAXBUTTON => Some(app::Hit::CaptionMax),
        HTCLOSE => Some(app::Hit::CaptionClose),
        _ => None,
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // the frame is the same before the app exists and while it is busy
    if msg == WM_NCCALCSIZE {
        return nc_calc_size(hwnd, wp, lp);
    }
    if app_ptr(hwnd).is_null() {
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    if BUSY.with(|b| b.get()) {
        // re-entrant message: defer
        match msg {
            WM_PAINT => {
                let mut ps: PAINTSTRUCT = std::mem::zeroed();
                BeginPaint(hwnd, &mut ps);
                EndPaint(hwnd, &ps);
                PENDING.with(|p| p.set(p.get() | P_PAINT));
                return 0;
            }
            app::WM_JOB => {
                PENDING.with(|p| p.set(p.get() | P_JOB));
                return 0;
            }
            app::WM_FS => {
                PENDING.with(|p| p.set(p.get() | P_FS));
                return 0;
            }
            app::WM_TERM => {
                PENDING.with(|p| p.set(p.get() | P_TERM));
                return 0;
            }
            WM_COMMAND => {
                PostMessageW(hwnd, DEFERRED_EDIT, wp, lp);
                return 0;
            }
            WM_CLOSE => {
                PENDING.with(|p| p.set(p.get() | P_CLOSE));
                return 0;
            }
            WM_SIZE => {
                PENDING.with(|p| p.set(p.get() | P_LAYOUT));
                return 0;
            }
            WM_DPICHANGED => {
                let r = &*(lp as *const RECT);
                PENDING.with(|p| p.set(p.get() | P_LAYOUT));
                SetWindowPos(hwnd, null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                return 0;
            }
            // handled below: with the app busy it blocks the shutdown rather than lose edits
            WM_CTLCOLOREDIT | WM_QUERYENDSESSION => {}
            WM_NCHITTEST => return nc_hit_test(hwnd, lp, true),
            _ => return DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);
            with_app(hwnd, |a| a.paint(hdc));
            EndPaint(hwnd, &ps);
            0
        }
        WM_ERASEBKGND => 1,
        WM_NCHITTEST => nc_hit_test(hwnd, lp, false),
        WM_NCMOUSEMOVE => {
            // hover of the caption buttons, which are non-client (HTMINBUTTON, ...) areas
            let hit = caption_button_hit(wp);
            if hit.is_some() {
                let mut tme = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE | TME_NONCLIENT, hwndTrack: hwnd, dwHoverTime: 0 };
                TrackMouseEvent(&mut tme);
            }
            with_app(hwnd, |a| {
                // the tooltip of a caption button shows at the mouse
                let mut p = POINT { x: lo(lp), y: hi(lp) };
                ScreenToClient(hwnd, &mut p);
                a.mouse = (p.x, p.y);
                a.set_hover(hit.unwrap_or(app::Hit::None));
            });
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_NCMOUSELEAVE => {
            with_app(hwnd, |a| {
                if matches!(a.hover, app::Hit::CaptionMin | app::Hit::CaptionMax | app::Hit::CaptionClose) {
                    a.set_hover(app::Hit::None);
                }
            });
            0
        }
        // the caption buttons act on release, like the native ones; the default handling would
        // draw classic buttons over the title bar
        WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK if caption_button_hit(wp).is_some() => {
            with_app(hwnd, |a| a.caption_down = wp as u32);
            0
        }
        WM_NCLBUTTONUP if caption_button_hit(wp).is_some() => {
            let down = with_app(hwnd, |a| std::mem::take(&mut a.caption_down)).unwrap_or(0);
            if down == wp as u32 {
                let sc = match wp as u32 {
                    HTMINBUTTON => SC_MINIMIZE,
                    HTMAXBUTTON if IsZoomed(hwnd) != 0 => SC_RESTORE,
                    HTMAXBUTTON => SC_MAXIMIZE,
                    _ => SC_CLOSE,
                };
                PostMessageW(hwnd, WM_SYSCOMMAND, sc as usize, 0);
            }
            0
        }
        WM_SIZE => {
            with_app(hwnd, |a| {
                if wp as u32 == SIZE_MINIMIZED {
                    a.release_buffer();
                } else {
                    a.layout();
                }
            });
            0
        }
        WM_DPICHANGED => {
            let dpi = (wp & 0xFFFF) as u32;
            let r = &*(lp as *const RECT);
            with_app(hwnd, |a| apply_dpi(a, dpi));
            SetWindowPos(hwnd, null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
            0
        }
        WM_GETMINMAXINFO => {
            let mmi = &mut *(lp as *mut MINMAXINFO);
            // logical pixels, like the layout
            let k = GetDpiForWindow(hwnd).max(96) as i32;
            mmi.ptMinTrackSize = POINT { x: 640 * k / 96, y: 400 * k / 96 };
            0
        }
        app::WM_JOB => {
            with_app(hwnd, |a| a.on_jobs());
            0
        }
        app::WM_FS => {
            arm_fs_timer(hwnd);
            0
        }
        app::WM_TERM => {
            with_app(hwnd, |a| a.on_term_data());
            0
        }
        WM_SYSCHAR if wp == 0x20 => DefWindowProcW(hwnd, msg, wp, lp),
        WM_CHAR | WM_SYSCHAR => {
            let alt = msg == WM_SYSCHAR;
            let handled = with_app(hwnd, |a| {
                if a.term_has_focus() {
                    a.term_char(wp as u16, alt);
                    true
                } else if !alt && a.editor_focused() {
                    a.editor_char(wp as u16);
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false);
            if handled {
                0
            } else {
                DefWindowProcW(hwnd, msg, wp, lp)
            }
        }
        WM_TIMER => {
            let id = wp;
            with_app(hwnd, |a| match id {
                app::TIMER_FS => unsafe {
                    KillTimer(hwnd, app::TIMER_FS);
                    FS_ARMED.with(|f| f.set(false));
                    a.refresh();
                },
                app::TIMER_TOAST => a.tick_toasts(),
                app::TIMER_TIP => a.show_tip(),
                app::TIMER_SPIN => {
                    a.spin = a.spin.wrapping_add(1);
                    a.invalidate();
                }
                _ => {}
            });
            0
        }
        WM_ACTIVATE => {
            let activating = (wp & 0xFFFF) != 0;
            let minimized = (wp >> 16) & 0xFFFF != 0;
            let restored = with_app(hwnd, |a| {
                if activating && a.repo.is_some() {
                    unsafe {
                        SetTimer(hwnd, app::TIMER_FS, 150, None);
                    }
                }
                // the title bar dims while the window is inactive
                a.invalidate();
                // like VS Code, Alt+Tab back returns to the commit box or quick pick that had
                // the keyboard (DefWindowProc would focus the main window)
                if !activating {
                    let f = unsafe { GetFocus() };
                    a.saved_focus = if f == a.commit_edit || f == a.overlay_edit { f } else { std::ptr::null_mut() };
                    false
                } else if !minimized && !a.saved_focus.is_null() && unsafe { IsWindowVisible(a.saved_focus) } != 0 {
                    unsafe { SetFocus(a.saved_focus) };
                    a.saved_focus = std::ptr::null_mut();
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false);
            if restored {
                return 0;
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        // a lone Alt would enter the (invisible) menu bar mode and swallow the next key;
        // Alt+Space (lp == ' ') still opens the window menu
        WM_SYSCOMMAND if (wp & 0xFFF0) as u32 == SC_KEYMENU && lp == 0 => 0,
        // Windows is shutting down, restarting or logging off: files with unsaved changes
        // block it with a reason and the usual question, like closing the window
        WM_QUERYENDSESSION => {
            let dirty = with_app(hwnd, |a| {
                let dirty = a.dirty_tab_keys();
                if dirty.is_empty() {
                    return false;
                }
                let reason = win::wz("Files have unsaved changes");
                unsafe {
                    windows_sys::Win32::System::Shutdown::ShutdownBlockReasonCreate(hwnd, reason.as_ptr());
                }
                if a.overlay.is_none() {
                    a.confirm_unsaved(dirty, |a| unsafe {
                        windows_sys::Win32::System::Shutdown::ShutdownBlockReasonDestroy(a.hwnd);
                        PostMessageW(a.hwnd, WM_CLOSE, 0, 0);
                    });
                }
                true
            })
            // None: a context menu or folder dialog is open (its modal loop holds the app), so
            // unsaved files can't be checked: block too rather than risk losing them
            .unwrap_or_else(|| {
                let reason = win::wz("Close the open menu or dialog first");
                unsafe { windows_sys::Win32::System::Shutdown::ShutdownBlockReasonCreate(hwnd, reason.as_ptr()) };
                true
            });
            if dirty {
                return 0;
            }
            windows_sys::Win32::System::Shutdown::ShutdownBlockReasonDestroy(hwnd);
            1
        }
        WM_ENDSESSION => {
            if wp != 0 {
                with_app(hwnd, |a| save_placement(hwnd, a));
            }
            0
        }
        WM_SETFOCUS | WM_KILLFOCUS => {
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_MOUSEMOVE => {
            let mut tme = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
            TrackMouseEvent(&mut tme);
            with_app(hwnd, |a| a.on_mouse_move(lo(lp), hi(lp)));
            0
        }
        WM_MOUSELEAVE => {
            with_app(hwnd, |a| a.set_hover(app::Hit::None));
            0
        }
        WM_SETCURSOR => {
            if (lp & 0xFFFF) as u32 == HTCLIENT {
                let mut p = POINT { x: 0, y: 0 };
                GetCursorPos(&mut p);
                ScreenToClient(hwnd, &mut p);
                let cur = with_app(hwnd, |a| a.cursor_for(p.x, p.y)).flatten();
                if let Some(c) = cur {
                    SetCursor(LoadCursorW(null_mut(), c));
                    return 1;
                }
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_LBUTTONDOWN => {
            let (ctrl, shift) = ((wp & 0x0008usize) != 0, (wp & 0x0004usize) != 0);
            with_app(hwnd, |a| a.on_lbutton_down(lo(lp), hi(lp), ctrl, shift));
            0
        }
        WM_LBUTTONUP => {
            with_app(hwnd, |a| a.on_lbutton_up());
            0
        }
        WM_LBUTTONDBLCLK => {
            let (ctrl, shift) = ((wp & 0x0008usize) != 0, (wp & 0x0004usize) != 0);
            with_app(hwnd, |a| a.on_dblclick(lo(lp), hi(lp), ctrl, shift));
            0
        }
        WM_MBUTTONUP => {
            with_app(hwnd, |a| a.on_mbutton_up(lo(lp), hi(lp)));
            0
        }
        WM_RBUTTONUP => {
            with_app(hwnd, |a| a.on_rbutton_up(lo(lp), hi(lp)));
            0
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            let delta = ((wp >> 16) & 0xFFFF) as i16 as i32;
            let mut p = POINT { x: lo(lp), y: hi(lp) };
            ScreenToClient(hwnd, &mut p);
            let horizontal = msg == WM_MOUSEHWHEEL || (wp & 0x0004usize) != 0;
            let delta = if msg == WM_MOUSEHWHEEL { -delta } else { delta };
            with_app(hwnd, |a| a.on_wheel(p.x, p.y, delta, horizontal));
            0
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let vk = wp as u16;
            let (ctrl, shift) = (key_down(VK_CONTROL), key_down(VK_SHIFT));
            if msg == WM_SYSKEYDOWN {
                let term_focus = with_app(hwnd, |a| a.term_has_focus()).unwrap_or(false);
                // Alt+Space opens the window menu, also from the terminal
                if vk == VK_F4 || vk == VK_SPACE || !(term_focus || vk == VK_F10) {
                    return DefWindowProcW(hwnd, msg, wp, lp);
                }
            }
            // AltGr characters reach WM_CHAR (the editor, the terminal) instead of running shortcuts
            if altgr_char(vk) {
                return 0;
            }
            with_app(hwnd, |a| a.on_key(vk, ctrl, shift));
            0
        }
        WM_COMMAND | DEFERRED_EDIT => {
            let code = ((wp >> 16) & 0xFFFF) as u32;
            let ctl = lp as HWND;
            if code == EN_CHANGE {
                with_app(hwnd, |a| {
                    if ctl == a.commit_edit {
                        a.edit_changed();
                    } else if ctl == a.overlay_edit {
                        a.filter_pick();
                    }
                });
            } else if code == EN_SETFOCUS || code == EN_KILLFOCUS {
                InvalidateRect(hwnd, null(), 0);
            }
            0
        }
        WM_CTLCOLOREDIT => {
            let p = app_ptr(hwnd);
            let a = &*p;
            let hdc = wp as HDC;
            SetTextColor(hdc, a.t.fg);
            SetBkColor(hdc, a.t.input_bg);
            a.edit_brush as LRESULT
        }
        WM_CLOSE => {
            // files with unsaved changes: ask first; the answer closes the window again
            let asked = with_app(hwnd, |a| {
                let dirty = a.dirty_tab_keys();
                if dirty.is_empty() {
                    return false;
                }
                a.confirm_unsaved(dirty, |a| unsafe {
                    PostMessageW(a.hwnd, WM_CLOSE, 0, 0);
                });
                true
            })
            .unwrap_or(false);
            if asked {
                return 0;
            }
            with_app(hwnd, |a| save_placement(hwnd, a));
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn apply_dpi(a: &mut App, dpi: u32) {
    a.dpi = dpi;
    a.scale = dpi as f32 / 96.0;
    a.fonts = win::Fonts::new(a.scale);
    set_edit_fonts(a);
    a.release_buffer();
}

/// AltGr reports Ctrl+Alt: a character typed with it (Polish ą, Hungarian @ and |, German {
/// ...) must not run Ctrl+letter shortcuts (Ctrl+A, Ctrl+W, Ctrl+V ...) anywhere. No shortcut
/// uses Ctrl+Alt with a character key; Ctrl+Alt+` stays a key.
fn altgr_char(vk: u16) -> bool {
    matches!(vk, 0x30..=0x5A | 0xBA..=0xE2) && vk != 0xC0 && key_down(VK_CONTROL) && key_down(VK_MENU)
}

/// Remembers the window's normal position and maximized state for the next start.
fn save_placement(hwnd: HWND, a: &mut App) {
    let mut wp: WINDOWPLACEMENT = unsafe { std::mem::zeroed() };
    wp.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
    let mut mi: MONITORINFO = unsafe { std::mem::zeroed() };
    mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    unsafe {
        GetWindowPlacement(hwnd, &mut wp);
        GetMonitorInfoW(MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY), &mut mi);
    }
    // rcNormalPosition is in workspace coordinates (relative to the primary work area) but
    // CreateWindowExW takes screen coordinates: with the taskbar at the top or left the window
    // would move by the taskbar's size on every start
    let (dx, dy) = (mi.rcWork.left - mi.rcMonitor.left, mi.rcWork.top - mi.rcMonitor.top);
    let r = wp.rcNormalPosition;
    // closed while minimized (taskbar > Close window): keep the maximized state it restores to
    let max = wp.showCmd == SW_SHOWMAXIMIZED as u32 || wp.showCmd == SW_SHOWMINIMIZED as u32 && wp.flags & WPF_RESTORETOMAXIMIZED != 0;
    a.st.window = Some((r.left + dx, r.top + dy, r.right - r.left, r.bottom - r.top, max));
    a.st.save();
}

/// Subclass for the commit message box (multi-line EDIT).
unsafe extern "system" fn commit_edit_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, main: usize) -> LRESULT {
    let main = main as HWND;
    match msg {
        WM_KEYDOWN => {
            let vk = wp as u16;
            let (ctrl, shift) = (key_down(VK_CONTROL), key_down(VK_SHIFT));
            if vk == VK_ESCAPE {
                SetFocus(main);
                return 0;
            }
            if altgr_char(vk) {
                return DefSubclassProc(hwnd, msg, wp, lp);
            }
            if ctrl && !shift && vk == 0x41 {
                SendMessageW(hwnd, 0x00B1, 0, -1);
                return 0;
            }
            if (ctrl || vk >= VK_F1 && vk <= VK_F12) && with_app(main, |a| a.global_key(vk, ctrl, shift)).unwrap_or(false) {
                return 0;
            }
        }
        WM_CHAR => {
            // swallow Ctrl+Enter (LF) and Ctrl+A / Ctrl+Backspace control chars
            if (wp == 0x0A) || (wp == 0x01) || (wp == 0x7F) {
                return 0;
            }
        }
        WM_PAINT => {
            let r = DefSubclassProc(hwnd, msg, wp, lp);
            if GetWindowTextLengthW(hwnd) == 0 {
                let p = app_ptr(main);
                if !p.is_null() {
                    let a = &*p;
                    let placeholder = match a.status() {
                        Some(s) => match &s.branch {
                            Some(b) => format!("Message (Ctrl+Enter to commit on \"{b}\")"),
                            None => "Message (Ctrl+Enter to commit)".to_string(),
                        },
                        None => String::new(),
                    };
                    let dc = GetDC(hwnd);
                    let g = win::Gfx { dc };
                    let mut rc = win::rect(0, 0, 0, 0);
                    GetClientRect(hwnd, &mut rc);
                    let m = (SendMessageW(hwnd, 0x00D4, 0, 0) & 0xFFFF) as i32;
                    g.text_in(&win::rect(rc.left + m, rc.top, rc.right, rc.top + a.s(18)), &placeholder, a.fonts.ui, crate::theme::blend_colorref(a.t.input_bg, a.t.fg, 0.55), win::DT_LINE);
                    ReleaseDC(hwnd, dc);
                }
            }
            return r;
        }
        _ => {}
    }
    DefSubclassProc(hwnd, msg, wp, lp)
}

/// Subclass for the quick pick / input box filter (single-line EDIT).
unsafe extern "system" fn overlay_edit_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, main: usize) -> LRESULT {
    let main = main as HWND;
    match msg {
        WM_KEYDOWN => {
            let vk = wp as u16;
            if matches!(vk, 0x26 | 0x28 | 0x21 | 0x22 | 0x0D | 0x1B) && with_app(main, |a| a.overlay_key(vk)).unwrap_or(false) {
                return 0;
            }
            if key_down(VK_CONTROL) && !key_down(VK_SHIFT) && vk == 0x41 && !altgr_char(vk) {
                SendMessageW(hwnd, 0x00B1, 0, -1);
                return 0;
            }
        }
        WM_CHAR => {
            if wp == 0x0D || wp == 0x1B || wp == 0x01 || wp == 0x7F {
                return 0;
            }
        }
        WM_KILLFOCUS => {
            // clicking elsewhere closes the overlay (unless it moved to the main window for list clicks)
        }
        _ => {}
    }
    DefSubclassProc(hwnd, msg, wp, lp)
}

/// Keeps a saved window rectangle on screen: moves it into the work area of the nearest
/// monitor and shrinks it to fit, so a position saved on a monitor that is no longer
/// connected (or at a different scale) doesn't open the window off-screen.
fn fit_to_monitor(x: i32, y: i32, w: i32, h: i32) -> (i32, i32, i32, i32) {
    unsafe {
        let r = RECT { left: x, top: y, right: x + w.max(1), bottom: y + h.max(1) };
        let mon = MonitorFromRect(&r, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if mon.is_null() || GetMonitorInfoW(mon, &mut mi) == 0 {
            return (CW_USEDEFAULT, CW_USEDEFAULT, 1280, 820);
        }
        let wa = mi.rcWork;
        let w = w.clamp(400, (wa.right - wa.left).max(400));
        let h = h.clamp(300, (wa.bottom - wa.top).max(300));
        let x = x.clamp(wa.left, (wa.right - w).max(wa.left));
        let y = y.clamp(wa.top, (wa.bottom - h).max(wa.top));
        (x, y, w, h)
    }
}

fn set_edit_fonts(a: &App) {
    unsafe {
        SendMessageW(a.commit_edit, WM_SETFONT, a.fonts.ui as WPARAM, 1);
        SendMessageW(a.overlay_edit, WM_SETFONT, a.fonts.ui as WPARAM, 1);
        let m = a.s(2) as isize;
        SendMessageW(a.commit_edit, 0x00D3, 3, m | (m << 16));
    }
}

fn main() {
    // ---- command line (code.exe style)
    let cli = cli::parse(std::env::args_os().skip(1).map(|a| a.to_string_lossy().into_owned()));
    if cli.help {
        cli::output("DevDeck", cli::USAGE);
        return;
    }
    if cli.version {
        cli::output("DevDeck", &format!("{}
{}
", env!("CARGO_PKG_VERSION"), git::version().map(|v| format!("git {v}")).unwrap_or_else(|| "git not found".into())));
        return;
    }
    if cli.install_menu {
        match cli::install_context_menu() {
            Ok(()) => cli::output("DevDeck", "Added \"Open with DevDeck\" to the Explorer folder context menu.
"),
            Err(e) => cli::output("DevDeck", &format!("Failed: {e}
")),
        }
        return;
    }
    if cli.uninstall_menu {
        cli::uninstall_context_menu();
        cli::output("DevDeck", "Removed the Explorer context menu entry.
");
        return;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let mut targets = Vec::new();
    let mut errors: Vec<String> = cli.unknown.iter().map(|u| format!("Ignoring unknown option '{u}'. Run with --help for usage.")).collect();
    for p in &cli.paths {
        match cli::resolve(p, &cwd) {
            Ok(t) => targets.push(t),
            Err(e) => errors.push(e),
        }
    }
    // every additional path gets its own window (process)
    if targets.len() > 1 {
        if let Ok(exe) = std::env::current_exe() {
            for t in targets.drain(1..) {
                let arg = t.file.as_ref().unwrap_or(&t.folder).display().to_string();
                let _ = std::process::Command::new(&exe).arg(arg).spawn();
            }
        }
    }
    let target = targets.into_iter().next();

    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let hinst = GetModuleHandleW(null());
        let class = win::wz("DevDeckWindow");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: win::app_icon(hinst, false),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: class.as_ptr(),
            hIconSm: win::app_icon(hinst, true),
        };
        RegisterClassExW(&wc);
        let st = settings::Settings::load();
        let (x, y, w, h, max) = match st.window {
            Some((x, y, w, h, max)) => {
                let (x, y, w, h) = fit_to_monitor(x, y, w, h);
                (x, y, w, h, max)
            }
            None => (CW_USEDEFAULT, CW_USEDEFAULT, 1280, 820, false),
        };
        let title = win::wz("DevDeck");
        let hwnd = CreateWindowExW(0, class.as_ptr(), title.as_ptr(), WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN, x, y, w, h, null_mut(), null_mut(), hinst, null());
        if hwnd.is_null() {
            return;
        }
        // recomputes the frame without the native caption (see `nc_calc_size`)
        SetWindowPos(hwnd, null_mut(), 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        let dpi = GetDpiForWindow(hwnd);
        let mut app = Box::new(App::new(hwnd, dpi, st));
        app.git_version = git::version();

        let edit_class = win::wz("EDIT");
        let empty = win::wz("");
        app.commit_edit = CreateWindowExW(0, edit_class.as_ptr(), empty.as_ptr(), WS_CHILD | WS_VISIBLE | ES_MULTILINE as u32 | ES_AUTOVSCROLL as u32 | ES_WANTRETURN as u32, 0, 0, 10, 10, hwnd, 100 as HMENU, hinst, null());
        app.overlay_edit = CreateWindowExW(0, edit_class.as_ptr(), empty.as_ptr(), WS_CHILD | ES_AUTOHSCROLL as u32, 0, 0, 10, 10, hwnd, 101 as HMENU, hinst, null());
        let main_ptr = hwnd as usize;
        SetWindowSubclass(app.commit_edit, Some(commit_edit_proc), 1, main_ptr);
        SetWindowSubclass(app.overlay_edit, Some(overlay_edit_proc), 2, main_ptr);
        set_edit_fonts(&app);
        app.apply_theme();

        let raw = Box::into_raw(app);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);

        // open folder from the command line, or the last used one
        let had_args = !cli.paths.is_empty();
        with_app(hwnd, |a| {
            if a.git_version.is_none() {
                a.error("Git not found. Install Git for Windows (https://git-scm.com) and make sure it is on PATH.");
            }
            for e in &errors {
                a.error(e.clone());
            }
            match target {
                Some(t) => a.open_target(t),
                None if had_args || cli.new_window => {}
                None => {
                    if let Some(last) = a.st.recent.first().cloned() {
                        if std::path::Path::new(&last).is_dir() && a.git_version.is_some() {
                            a.open_repo(std::path::Path::new(&last));
                        }
                    }
                }
            }
            a.layout();
            // restore the terminal like VS Code does when it was open last time
            if a.terminal_visible() {
                a.start_terminal(None);
            }
        });

        ShowWindow(hwnd, if max { SW_SHOWMAXIMIZED } else { SW_SHOW });
        UpdateWindow(hwnd);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = Box::from_raw(raw);
    }
}
