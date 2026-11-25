//! Work around a winit `WM_DPICHANGED` bug on multi-monitor setups with
//! different DPI scales: when a window drag crosses onto another monitor,
//! winit repositions the window from Windows' suggested rect, which lands
//! somewhere other than where the user is dragging when the monitors are
//! offset from each other. Subclassing the HWND catches the message in real
//! time and puts the window back where the cursor says it should be.

#![cfg(target_os = "windows")]

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use winit::window::Window;

static OLD_WNDPROC: AtomicUsize = AtomicUsize::new(0);
static INSTALLED_HWND: AtomicUsize = AtomicUsize::new(0);
static DRAGGING: AtomicBool = AtomicBool::new(false);
/// cursor offset from the window's top-left corner, captured at
/// WM_ENTERSIZEMOVE — preserved across DPI changes so the window stays
/// glued to the cursor exactly like a native drag
static GRAB_DX: AtomicI32 = AtomicI32::new(0);
static GRAB_DY: AtomicI32 = AtomicI32::new(0);

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        let old: WNDPROC = std::mem::transmute::<usize, WNDPROC>(OLD_WNDPROC.load(Ordering::SeqCst));
        match msg {
            WM_ENTERSIZEMOVE => {
                let mut pt: POINT = std::mem::zeroed();
                let mut rc: RECT = std::mem::zeroed();
                GetCursorPos(&mut pt);
                GetWindowRect(hwnd, &mut rc);
                GRAB_DX.store(pt.x - rc.left, Ordering::SeqCst);
                GRAB_DY.store(pt.y - rc.top, Ordering::SeqCst);
                DRAGGING.store(true, Ordering::SeqCst);
            }
            WM_EXITSIZEMOVE => DRAGGING.store(false, Ordering::SeqCst),
            WM_DPICHANGED => {
                // let winit handle it first (scale factor + its own rect), then
                // put the window back where the cursor says it should be
                let r = CallWindowProcW(old, hwnd, msg, wparam, lparam);
                if DRAGGING.load(Ordering::SeqCst) {
                    let mut pt: POINT = std::mem::zeroed();
                    GetCursorPos(&mut pt);
                    SetWindowPos(
                        hwnd,
                        std::ptr::null_mut(),
                        pt.x - GRAB_DX.load(Ordering::SeqCst),
                        pt.y - GRAB_DY.load(Ordering::SeqCst),
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
                return r;
            }
            _ => {}
        }
        CallWindowProcW(old, hwnd, msg, wparam, lparam)
    }
}

/// Subclass the window's HWND once. Safe to call after every (re)creation:
/// a new HWND gets subclassed, an already-hooked one is a no-op.
pub fn install(window: &Window) {
    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(h) = handle.as_raw() else {
        return;
    };
    let hwnd = h.hwnd.get() as usize;
    if INSTALLED_HWND.load(Ordering::SeqCst) == hwnd {
        return;
    }
    unsafe {
        let proc_fn: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT = wndproc;
        let old = SetWindowLongPtrW(hwnd as HWND, GWLP_WNDPROC, proc_fn as usize as isize);
        if old != 0 {
            OLD_WNDPROC.store(old as usize, Ordering::SeqCst);
            INSTALLED_HWND.store(hwnd, Ordering::SeqCst);
        }
    }
}
