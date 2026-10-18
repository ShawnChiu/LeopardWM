//! Active window border frame overlay.
//!
//! Renders a colored frame around the focused window using a layered window
//! with per-pixel alpha for anti-aliased rounded corners. Uses a signed
//! distance field to produce smooth edges without GDI+ or Direct2D.

use std::cell::Cell;
use std::ffi::c_void;
use std::sync::mpsc;
use std::sync::Mutex;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWINDOWATTRIBUTE};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::Win32Error;

/// Position of the border relative to the window frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderPosition {
    Outside,
    Inside,
}

/// Default corner radius (pixels) when the caller doesn't pass a per-window
/// radius. Matches Windows 11's `DWMWCP_ROUND` default.
pub const DEFAULT_CORNER_RADIUS: f32 = 8.0;

/// Signed distance field for a rounded rectangle.
///
/// Returns negative values inside, positive outside, zero on the boundary.
pub(crate) fn rounded_rect_sdf(
    px: f32,
    py: f32,
    rx: f32,
    ry: f32,
    rw: f32,
    rh: f32,
    radius: f32,
) -> f32 {
    let cx = rx + rw / 2.0;
    let cy = ry + rh / 2.0;
    let hx = rw / 2.0;
    let hy = rh / 2.0;

    let dx = (px - cx).abs() - hx + radius;
    let dy = (py - cy).abs() - hy + radius;

    let outside = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt();
    let inside = dx.max(dy).min(0.0);
    outside + inside - radius
}

pub(crate) fn clamp(v: f32, lo: f32, hi: f32) -> f32 {
    v.max(lo).min(hi)
}

/// Z-order plan for stacking the overlay immediately above its target.
///
/// `insert_after` is passed to `SetWindowPos` as `hWndInsertAfter`. `None` keeps
/// the current z-order (`SWP_NOZORDER`). `Some(HWND_TOP)` is a sentinel, not an
/// invalid window: it is distinct from `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OverlayStackPlan {
    /// Demote with `HWND_NOTOPMOST` before applying `insert_after`.
    demote: bool,
    insert_after: Option<HWND>,
}

fn window_is_topmost(hwnd: HWND) -> bool {
    let ex_style = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32;
    ex_style & WS_EX_TOPMOST.0 != 0
}

fn window_above(hwnd: HWND) -> Option<HWND> {
    unsafe {
        match GetWindow(hwnd, GW_HWNDPREV) {
            Ok(prev) if !prev.is_invalid() => Some(prev),
            _ => None,
        }
    }
}

fn apply_style(color_bgr: u32, corner_radius: f32) {
    if let Ok(mut state) = BORDER_STATE.lock() {
        state.color_bgr = color_bgr;
        state.corner_radius = corner_radius;
    }
}

fn overlay_needs_render(w: i32, h: i32, width: u32, position: BorderPosition) -> bool {
    let state = BORDER_STATE.lock().unwrap();
    w != state.cached_w
        || h != state.cached_h
        || width != state.cached_width
        || position != state.cached_position
        || state.color_bgr != state.cached_color
        || (state.corner_radius - state.cached_corner_radius).abs() > f32::EPSILON
}

/// Decide how to stack `overlay` immediately above `target`.
///
/// Inserting after a topmost predecessor of a non-topmost target would promote
/// the overlay into the topmost band (and a later `HWND_TOP` would then raise it
/// over the taskbar). `HWND_TOP` is not `HWND_TOPMOST`, but it still raises a
/// window already in the topmost band to the top of that band.
fn plan_overlay_stack(
    overlay: HWND,
    overlay_is_topmost: bool,
    target_is_topmost: bool,
    predecessor: Option<HWND>,
    predecessor_is_topmost: bool,
) -> OverlayStackPlan {
    let demote = overlay_is_topmost && !target_is_topmost;
    if predecessor == Some(overlay) {
        return OverlayStackPlan {
            demote,
            insert_after: None,
        };
    }
    let insert_after = match predecessor {
        Some(prev) if predecessor_is_topmost == target_is_topmost => Some(prev),
        _ if target_is_topmost => Some(HWND_TOPMOST),
        _ => Some(HWND_TOP),
    };
    OverlayStackPlan {
        demote,
        insert_after,
    }
}

struct BorderState {
    color_bgr: u32,
    corner_radius: f32,
    cached_w: i32,
    cached_h: i32,
    cached_width: u32,
    cached_position: BorderPosition,
    cached_color: u32,
    cached_corner_radius: f32,
}

static BORDER_STATE: Mutex<BorderState> = Mutex::new(BorderState {
    color_bgr: 0x00F48542,
    corner_radius: DEFAULT_CORNER_RADIUS,
    cached_w: 0,
    cached_h: 0,
    cached_width: 0,
    cached_position: BorderPosition::Outside,
    cached_color: 0,
    cached_corner_radius: -1.0,
});

/// Manages a transparent overlay window that draws a colored border frame
/// around the focused window with anti-aliased rounded corners.
pub struct BorderFrame {
    hwnd: HWND,
    visible: Cell<bool>,
    _thread: Option<std::thread::JoinHandle<()>>,
}

impl BorderFrame {
    /// Create the border frame overlay on a background thread.
    pub fn new() -> Result<Self, Win32Error> {
        #[cfg(test)]
        panic!("BorderFrame::new spawns a layered DWM window; gate the call behind cfg(test)");
        #[allow(unreachable_code)]
        let (tx, rx) = mpsc::channel::<Result<isize, Win32Error>>();

        let thread = std::thread::Builder::new()
            .name("border-frame".into())
            .spawn(move || unsafe {
                let class_name: Vec<u16> = "LeopardWMBorderFrame\0".encode_utf16().collect();
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(border_frame_proc),
                    lpszClassName: windows::core::PCWSTR(class_name.as_ptr()),
                    ..Default::default()
                };
                RegisterClassW(&wc);

                let ex_style =
                    WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT;

                match CreateWindowExW(
                    ex_style,
                    windows::core::PCWSTR(class_name.as_ptr()),
                    None,
                    WS_POPUP,
                    0,
                    0,
                    1,
                    1,
                    None,
                    None,
                    None,
                    None,
                ) {
                    Ok(h) => {
                        let _ = tx.send(Ok(h.0 as isize));
                        let mut msg = MSG::default();
                        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                            let _ = DispatchMessageW(&msg);
                        }
                        let _ = DestroyWindow(h);
                        let _ = UnregisterClassW(windows::core::PCWSTR(class_name.as_ptr()), None);
                    }
                    Err(e) => {
                        let _ = tx.send(Err(Win32Error::HookInstallFailed(format!(
                            "BorderFrame: {}",
                            e
                        ))));
                    }
                }
            })
            .map_err(|e| Win32Error::HookInstallFailed(format!("BorderFrame thread: {}", e)))?;

        let hwnd_raw = match rx.recv() {
            Ok(Ok(raw)) => raw,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(Win32Error::HookInstallFailed(
                    "BorderFrame init failed".into(),
                ))
            }
        };

        Ok(Self {
            hwnd: HWND(hwnd_raw as *mut c_void),
            visible: Cell::new(false),
            _thread: Some(thread),
        })
    }

    /// Show the border frame around the target window.
    pub fn show(
        &self,
        target_hwnd: u64,
        width: u32,
        position: BorderPosition,
        color_bgr: u32,
        corner_radius: f32,
    ) {
        apply_style(color_bgr, corner_radius);
        self.reposition(target_hwnd, width, position);
    }

    /// Show the border at a specific screen rect, stacked above `target_hwnd`.
    /// Used during drag and resize preview to keep the border at the layout
    /// position while still tracking the target's z-order.
    pub fn show_at_rect(
        &self,
        rect: leopardwm_core_layout::Rect,
        width: u32,
        position: BorderPosition,
        color_bgr: u32,
        corner_radius: f32,
        target_hwnd: u64,
    ) {
        let Some(target) = self.live_target(target_hwnd) else {
            return;
        };
        apply_style(color_bgr, corner_radius);

        let bw = width as i32;
        // For outside borders, the layout rect (which matches the DWM-
        // reported visible bounds for a managed window) includes a 1 px
        // transparent resize border on every side. Shrink by 1 px so the
        // visible border sits flush with the actual window content
        // edge. Mirrors the same compensation in `reposition()`. Without
        // this the border has a 1 px gap from the window content (the
        // exact width of the transparent resize border).
        let (rx, ry, tw, th) = match position {
            BorderPosition::Outside => (
                rect.x + 1,
                rect.y + 1,
                (rect.width - 2).max(0),
                (rect.height - 2).max(0),
            ),
            BorderPosition::Inside => (rect.x, rect.y, rect.width, rect.height),
        };

        let (x, y, w, h) = match position {
            BorderPosition::Outside => (rx - bw, ry - bw, tw + 2 * bw, th + 2 * bw),
            BorderPosition::Inside => (rx, ry, tw, th),
        };

        self.present_overlay(
            leopardwm_core_layout::Rect::new(x, y, w, h),
            width,
            position,
            target,
        );
    }

    /// Show the border at an already-final overlay rectangle (no expansion),
    /// stacked above `target_hwnd`.
    pub fn show_final_overlay(
        &self,
        overlay: leopardwm_core_layout::Rect,
        width: u32,
        position: BorderPosition,
        color_bgr: u32,
        corner_radius: f32,
        target_hwnd: u64,
    ) {
        let Some(target) = self.live_target(target_hwnd) else {
            return;
        };
        apply_style(color_bgr, corner_radius);
        self.present_overlay(overlay, width, position, target);
    }

    /// Hide the border frame.
    pub fn hide(&self) {
        self.visible.set(false);
        unsafe {
            let _ = ShowWindowAsync(self.hwnd, SW_HIDE);
        }
    }

    fn live_target(&self, target_hwnd: u64) -> Option<HWND> {
        let target = HWND(target_hwnd as *mut c_void);
        if target.is_invalid() || unsafe { !IsWindow(Some(target)).as_bool() } {
            self.hide();
            None
        } else {
            Some(target)
        }
    }

    fn present_overlay(
        &self,
        overlay: leopardwm_core_layout::Rect,
        width: u32,
        position: BorderPosition,
        target: HWND,
    ) {
        let x = overlay.x;
        let y = overlay.y;
        let w = overlay.width;
        let h = overlay.height;
        if overlay_needs_render(w, h, width, position) {
            self.render_and_update(x, y, w, h, width, position);
        } else {
            unsafe {
                let _ = SetWindowPos(
                    self.hwnd,
                    None,
                    x,
                    y,
                    0,
                    0,
                    SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOSIZE | SWP_ASYNCWINDOWPOS,
                );
            }
        }
        self.stack_above_target(target);
    }

    fn stack_above_target(&self, target: HWND) {
        if target.is_invalid() || unsafe { !IsWindow(Some(target)).as_bool() } {
            self.hide();
            return;
        }
        let predecessor = window_above(target);
        let plan = plan_overlay_stack(
            self.hwnd,
            window_is_topmost(self.hwnd),
            window_is_topmost(target),
            predecessor,
            predecessor.is_some_and(window_is_topmost),
        );
        unsafe {
            if plan.demote {
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_NOTOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS,
                );
            }
            let z_flags = if plan.insert_after.is_some() {
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS
            } else {
                SWP_NOZORDER | SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS
            };
            let _ = SetWindowPos(self.hwnd, plan.insert_after, 0, 0, 0, 0, z_flags);
            if !self.visible.replace(true) {
                let _ = ShowWindowAsync(self.hwnd, SW_SHOWNA);
            }
        }
    }

    /// Reposition the border frame to track the target window.
    ///
    /// If the overlay dimensions and config haven't changed since the last
    /// render, only the position is updated (fast path for scrolling).
    pub fn reposition(&self, target_hwnd: u64, width: u32, position: BorderPosition) {
        let Some(target) = self.live_target(target_hwnd) else {
            return;
        };
        let bw = width as i32;

        unsafe {
            // Get actual visible window bounds (excludes DWM shadow padding).
            let mut rect = RECT::default();
            const DWMWA_EXTENDED_FRAME_BOUNDS: i32 = 9;
            let got_dwm = DwmGetWindowAttribute(
                target,
                DWMWINDOWATTRIBUTE(DWMWA_EXTENDED_FRAME_BOUNDS),
                &mut rect as *mut RECT as *mut c_void,
                std::mem::size_of::<RECT>() as u32,
            )
            .is_ok();
            if !got_dwm && GetWindowRect(target, &mut rect).is_err() {
                return;
            }

            // For outside borders, DWMWA_EXTENDED_FRAME_BOUNDS includes a
            // transparent resize border. Shrink by 1px to match the actual
            // visual window edge so the border sits flush with the content.
            if position == BorderPosition::Outside {
                rect.left += 1;
                rect.top += 1;
                rect.right -= 1;
                rect.bottom -= 1;
            }

            let tw = rect.right - rect.left;
            let th = rect.bottom - rect.top;

            let (x, y, w, h) = match position {
                BorderPosition::Outside => {
                    (rect.left - bw, rect.top - bw, tw + 2 * bw, th + 2 * bw)
                }
                BorderPosition::Inside => (rect.left, rect.top, tw, th),
            };

            self.present_overlay(
                leopardwm_core_layout::Rect::new(x, y, w, h),
                width,
                position,
                target,
            );
        }
    }

    fn render_and_update(
        &self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        width: u32,
        position: BorderPosition,
    ) {
        let bw = width as f32;

        let (color_bgr, base_radius) = {
            let mut state = BORDER_STATE.lock().unwrap();
            state.cached_w = w;
            state.cached_h = h;
            state.cached_width = width;
            state.cached_position = position;
            state.cached_color = state.color_bgr;
            state.cached_corner_radius = state.corner_radius;
            (state.color_bgr, state.corner_radius)
        };

        let cb = ((color_bgr >> 16) & 0xFF) as u8;
        let cg = ((color_bgr >> 8) & 0xFF) as u8;
        let cr = (color_bgr & 0xFF) as u8;

        let (outer_r, inner_r) = match position {
            BorderPosition::Outside => {
                // Rect was shrunk by 1px, so visual radius is base - 1.
                let visual_r = (base_radius - 1.0).max(0.0);
                let outer = visual_r + bw;
                let inner = visual_r;
                (outer, inner)
            }
            BorderPosition::Inside => {
                let outer = base_radius;
                let inner = (base_radius - bw).max(0.0);
                (outer, inner)
            }
        };

        let wf = w as f32;
        let hf = h as f32;

        unsafe {
            // Create a 32-bit top-down DIB section for per-pixel alpha
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h, // negative = top-down
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };

            let mut bits: *mut c_void = std::ptr::null_mut();
            let hbitmap = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0);
            let Ok(hbitmap) = hbitmap else {
                return;
            };

            let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (w * h) as usize);

            // Only iterate over pixels near the border edges (optimization).
            // The band must be wide enough to capture corner rounding where the
            // inner SDF curves away, extending border pixels further inward.
            let margin = 1.5f32;
            let band = bw + inner_r + margin;

            for py in 0..h {
                let pyf = py as f32 + 0.5;
                let in_top_band = pyf < band;
                let in_bottom_band = pyf > hf - band;
                let in_vert_band = in_top_band || in_bottom_band;

                for px in 0..w {
                    let pxf = px as f32 + 0.5;

                    if !in_vert_band {
                        let in_left_band = pxf < band;
                        let in_right_band = pxf > wf - band;
                        if !in_left_band && !in_right_band {
                            continue;
                        }
                    }

                    let sdf_outer = rounded_rect_sdf(pxf, pyf, 0.0, 0.0, wf, hf, outer_r);
                    let sdf_inner =
                        rounded_rect_sdf(pxf, pyf, bw, bw, wf - 2.0 * bw, hf - 2.0 * bw, inner_r);

                    // Anti-aliased alpha: smooth transition at both edges
                    let alpha_outer = clamp(0.5 - sdf_outer, 0.0, 1.0);
                    let alpha_inner = clamp(sdf_inner + 0.5, 0.0, 1.0);
                    let alpha = alpha_outer * alpha_inner;

                    if alpha > 0.0 {
                        let a = (alpha * 255.0) as u8;
                        // Premultiplied alpha (required by UpdateLayeredWindow + AC_SRC_ALPHA)
                        let pr = (cr as u32 * a as u32 / 255) as u8;
                        let pg = (cg as u32 * a as u32 / 255) as u8;
                        let pb = (cb as u32 * a as u32 / 255) as u8;
                        // BGRA pixel (little-endian: B, G, R, A)
                        pixels[(py * w + px) as usize] =
                            (a as u32) << 24 | (pr as u32) << 16 | (pg as u32) << 8 | pb as u32;
                    }
                }
            }

            let hdc_screen = GetDC(None);
            let hdc_mem = CreateCompatibleDC(Some(hdc_screen));
            let old = SelectObject(hdc_mem, hbitmap.into());

            let pt_dst = POINT { x, y };
            let sz = SIZE { cx: w, cy: h };
            let pt_src = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };

            let _ = UpdateLayeredWindow(
                self.hwnd,
                Some(hdc_screen),
                Some(&pt_dst),
                Some(&sz),
                Some(hdc_mem),
                Some(&pt_src),
                windows::Win32::Foundation::COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );

            SelectObject(hdc_mem, old);
            let _ = DeleteDC(hdc_mem);
            ReleaseDC(None, hdc_screen);
            let _ = DeleteObject(hbitmap.into());
        }
    }
}

impl Drop for BorderFrame {
    fn drop(&mut self) {
        unsafe {
            let _ = PostMessageW(Some(self.hwnd), WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self._thread.take() {
            let _ = thread.join();
        }
    }
}

/// Minimal window proc — layered windows with UpdateLayeredWindow don't use WM_PAINT.
unsafe extern "system" fn border_frame_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::c_void;

    fn hwnd(id: usize) -> HWND {
        HWND(id as *mut c_void)
    }

    #[test]
    fn self_predecessor_does_not_raise() {
        let overlay = hwnd(0x100);
        let plan = plan_overlay_stack(overlay, false, false, Some(overlay), false);
        assert_eq!(
            plan,
            OverlayStackPlan {
                demote: false,
                insert_after: None,
            }
        );
    }

    #[test]
    fn self_predecessor_topmost_does_not_raise() {
        let overlay = hwnd(0x100);
        let plan = plan_overlay_stack(overlay, true, true, Some(overlay), true);
        assert_eq!(
            plan,
            OverlayStackPlan {
                demote: false,
                insert_after: None,
            }
        );
    }

    #[test]
    fn leftover_topmost_self_predecessor_demotes_without_raise() {
        let overlay = hwnd(0x100);
        let plan = plan_overlay_stack(overlay, true, false, Some(overlay), true);
        assert_eq!(
            plan,
            OverlayStackPlan {
                demote: true,
                insert_after: None,
            }
        );
    }

    #[test]
    fn ordinary_target_inserts_after_window_above_it() {
        let overlay = hwnd(0x100);
        let above = hwnd(0x200);
        let plan = plan_overlay_stack(overlay, false, false, Some(above), false);
        assert_eq!(
            plan,
            OverlayStackPlan {
                demote: false,
                insert_after: Some(above),
            }
        );
    }

    #[test]
    fn ordinary_target_with_shell_above_stays_non_topmost() {
        let overlay = hwnd(0x100);
        let shell = hwnd(0x400);
        let plan = plan_overlay_stack(overlay, false, false, Some(shell), true);
        assert_eq!(
            plan,
            OverlayStackPlan {
                demote: false,
                insert_after: Some(HWND_TOP),
            }
        );
        assert_ne!(plan.insert_after, Some(shell));
        assert_ne!(plan.insert_after, Some(HWND_TOPMOST));
    }

    #[test]
    fn target_switch_from_topmost_to_ordinary_demotes_below_shell() {
        let overlay = hwnd(0x100);
        let shell = hwnd(0x400);
        let plan = plan_overlay_stack(overlay, true, false, Some(shell), true);
        assert_eq!(
            plan,
            OverlayStackPlan {
                demote: true,
                insert_after: Some(HWND_TOP),
            }
        );
        assert_ne!(plan.insert_after, Some(shell));
        assert_ne!(plan.insert_after, Some(HWND_TOPMOST));
    }

    #[test]
    fn topmost_target_inserts_after_same_band_predecessor() {
        let overlay = hwnd(0x100);
        let above = hwnd(0x300);
        let plan = plan_overlay_stack(overlay, false, true, Some(above), true);
        assert_eq!(
            plan,
            OverlayStackPlan {
                demote: false,
                insert_after: Some(above),
            }
        );
        assert_ne!(plan.insert_after, Some(HWND_TOPMOST));
    }

    #[test]
    fn topmost_target_at_top_of_zorder_uses_topmost_sentinel() {
        let overlay = hwnd(0x100);
        let plan = plan_overlay_stack(overlay, false, true, None, false);
        assert_eq!(
            plan,
            OverlayStackPlan {
                demote: false,
                insert_after: Some(HWND_TOPMOST),
            }
        );
    }

    #[test]
    fn hwnd_top_is_distinct_from_no_zorder_change() {
        let overlay = hwnd(0x100);
        let keep = plan_overlay_stack(overlay, false, false, Some(overlay), false);
        let top_of_band = plan_overlay_stack(overlay, false, false, None, false);
        assert_eq!(keep.insert_after, None);
        assert_eq!(top_of_band.insert_after, Some(HWND_TOP));
        assert_ne!(keep.insert_after, top_of_band.insert_after);
    }
}
