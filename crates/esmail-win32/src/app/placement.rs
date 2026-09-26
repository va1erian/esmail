//! Reading and restoring the top-level window's placement, so it reopens where
//! and how it was left. xui-win32 has this on `Window` but not on `Ui`, so the
//! two Win32 calls are made here on the window's handle.

use core::mem::size_of;

use xui_win32::{Hwnd, Placement, Rect, ShowState};
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowPlacement, SW_SHOWMAXIMIZED, SW_SHOWNORMAL, SetForegroundWindow, SetWindowPlacement, WINDOWPLACEMENT, WINDOWPLACEMENT_FLAGS,
};

fn raw(hwnd: Hwnd) -> HWND {
    HWND(hwnd.raw() as *mut core::ffi::c_void)
}

/// Brings one of this app's own windows to the front.
pub fn bring_forward(hwnd: Hwnd) {
    // SAFETY: plain FFI on a handle of this process's own window; a refusal
    // (the window is gone) changes nothing.
    let _ = unsafe { SetForegroundWindow(raw(hwnd)) };
}

/// The window's restored rectangle (left, top, right, bottom in screen pixels)
/// and whether it is maximized. A minimized window reports its restored state.
pub fn read(hwnd: Hwnd) -> Option<([i32; 4], bool)> {
    let mut placement = WINDOWPLACEMENT { length: size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
    // SAFETY: `placement` is a valid out-pointer whose `length` field describes
    // its size, as `GetWindowPlacement` requires.
    unsafe { GetWindowPlacement(raw(hwnd), &mut placement) }.ok()?;
    let RECT { left, top, right, bottom } = placement.rcNormalPosition;
    Some(([left, top, right, bottom], placement.showCmd == SW_SHOWMAXIMIZED.0 as u32))
}

/// Moves the window to `bounds` (as [`read`] returned it), maximizing it when
/// asked. A rectangle that lies outside every monitor (the monitor it was on
/// is gone) is brought back onto the nearest one.
pub fn restore(hwnd: Hwnd, bounds: [i32; 4], maximized: bool) {
    let [left, top, right, bottom] = bounds;
    let placement = Placement { normal: Rect::new(left, top, right, bottom), show: ShowState::Normal }.clamp_to_work_areas();
    let value = WINDOWPLACEMENT {
        length: size_of::<WINDOWPLACEMENT>() as u32,
        flags: WINDOWPLACEMENT_FLAGS(0),
        showCmd: if maximized { SW_SHOWMAXIMIZED.0 as u32 } else { SW_SHOWNORMAL.0 as u32 },
        ptMinPosition: POINT { x: 0, y: 0 },
        ptMaxPosition: POINT { x: 0, y: 0 },
        rcNormalPosition: RECT { left: placement.normal.left, top: placement.normal.top, right: placement.normal.right, bottom: placement.normal.bottom },
    };
    // SAFETY: `value` is fully initialised with `length` set as required. A
    // failure leaves the window where it opened, which is a fine fallback.
    let _ = unsafe { SetWindowPlacement(raw(hwnd), &value) };
}
