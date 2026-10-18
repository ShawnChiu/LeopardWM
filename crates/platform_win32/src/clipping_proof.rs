//! Opt-in native proof for framed-window region clipping.
//!
//! This module is compiled only for tests. It is deliberately not a window-management
//! abstraction or a production clipping implementation.
//!
//! To supervise an opt-in proof externally, retain the built libtest executable itself, not a
//! Cargo parent: `$p = Start-Process -PassThru -FilePath <built-test-exe> -ArgumentList
//! '--exact','clipping_proof::framed_window_region_viability','--ignored','--nocapture'`; with
//! `LEOPARDWM_RUN_FRAMED_WINDOW_CLIPPING_PROOF=1`, stop only `$p.Id` if it exceeds 15 seconds.
//! For `clipping_proof::known_state_region_ownership_recovery_matrix`, retain that same exact
//! executable for 125 seconds; the matrix itself exits at 120 seconds, controllers at 12 seconds,
//! and fixtures at 30 seconds.

use std::ffi::c_void;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use windows::core::{BOOL, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, FILETIME, HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Dwm::{
    DwmFlush, DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS, DWMWA_NCRENDERING_ENABLED,
};
use windows::Win32::Graphics::Gdi::{
    CombineRgn, CreateRectRgn, DeleteObject, ExtCreateRegion, GetRegionData, GetWindowRgn,
    GetWindowRgnBox, SetWindowRgn, ERROR, HRGN, RGNDATA, RGN_AND, RGN_OR,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::HiDpi::{
    SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetPropW,
    GetWindowLongW, GetWindowRect, GetWindowThreadProcessId, IsWindow, IsWindowVisible,
    PeekMessageW, RegisterClassW, RemovePropW, SetPropW, SetWindowPos, ShowWindow,
    TranslateMessage, UnregisterClassW, GWL_EXSTYLE, GWL_STYLE, MSG, PM_REMOVE, SWP_NOACTIVATE,
    SWP_NOSIZE, SWP_NOZORDER, SW_HIDE, SW_SHOWNOACTIVATE, WNDCLASSW, WS_EX_LAYOUTRTL,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_OVERLAPPEDWINDOW,
};

const OPT_IN_ENV: &str = "LEOPARDWM_RUN_FRAMED_WINDOW_CLIPPING_PROOF";
const CHILD_ENV: &str = "LEOPARDWM_FRAMED_WINDOW_CLIPPING_PROOF_CHILD";
const TEST_NAME: &str = "clipping_proof::framed_window_region_viability";
const FIXTURE_DEADLINE: Duration = Duration::from_secs(12);
const SUPERVISOR_DEADLINE: Duration = Duration::from_secs(13);
const MATRIX_ROLE_ENV: &str = "LEOPARDWM_CLIPPING_MATRIX_ROLE";
const MATRIX_STATE_ENV: &str = "LEOPARDWM_CLIPPING_MATRIX_STATE";
const MATRIX_FIXTURE_ENV: &str = "LEOPARDWM_CLIPPING_MATRIX_FIXTURE";
const MATRIX_BOUNDARY_ENV: &str = "LEOPARDWM_CLIPPING_MATRIX_BOUNDARY";
const MATRIX_FAULT_ENV: &str = "LEOPARDWM_CLIPPING_MATRIX_FAULT";
const MATRIX_LAYOUT_ENV: &str = "LEOPARDWM_CLIPPING_MATRIX_LAYOUT";
const MATRIX_EXPECTED_FAULT_EXIT: i32 = 86;
const MATRIX_GENERATION_PROPERTY: &str = "LeopardWMClippingProofGeneration";
const MATRIX_OWNERSHIP_PROPERTY: &str = "LeopardWMClippingProofOwnership";
const MATRIX_TEST_NAME: &str = "clipping_proof::known_state_region_ownership_recovery_matrix";
const MATRIX_PROTOCOL_WAIT: Duration = Duration::from_secs(4);
const MATRIX_CONTROLLER_DEADLINE: Duration = Duration::from_secs(12);
const MATRIX_FIXTURE_DEADLINE: Duration = Duration::from_secs(30);
const MATRIX_SUPERVISOR_DEADLINE: Duration = Duration::from_secs(120);
const MATRIX_EXTERNAL_SUPERVISOR_DEADLINE: Duration = Duration::from_secs(125);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Bounds {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl Bounds {
    fn width(self) -> i32 {
        self.right - self.left
    }

    fn height(self) -> i32 {
        self.bottom - self.top
    }

    fn is_valid(self) -> bool {
        self.width() > 0 && self.height() > 0
    }

    fn relative_to(self, outer: Self) -> Self {
        Self {
            left: self.left - outer.left,
            top: self.top - outer.top,
            right: self.right - outer.left,
            bottom: self.bottom - outer.top,
        }
    }
}

impl From<RECT> for Bounds {
    fn from(rect: RECT) -> Self {
        Self {
            left: rect.left,
            top: rect.top,
            right: rect.right,
            bottom: rect.bottom,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OuterClientBounds {
    outer: Bounds,
    client: Bounds,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ComposedMeasurement {
    outer: Bounds,
    client: Bounds,
    extended_frame_relative_to_outer: Bounds,
    nc_rendering_enabled: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FramedWindowEvidence {
    hidden_baseline: OuterClientBounds,
    composed_baseline: ComposedMeasurement,
    clipped: ComposedMeasurement,
    cleared: ComposedMeasurement,
    installed_clip_bounds: Bounds,
    queried_clip_bounds: Bounds,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum MechanicalVerdict {
    Pass {
        presentation_limitations: Vec<&'static str>,
    },
    Fail(String),
}

fn evaluate(evidence: &FramedWindowEvidence) -> MechanicalVerdict {
    let measurements = [
        evidence.hidden_baseline.outer,
        evidence.hidden_baseline.client,
        evidence.composed_baseline.outer,
        evidence.composed_baseline.client,
        evidence.composed_baseline.extended_frame_relative_to_outer,
        evidence.clipped.outer,
        evidence.clipped.client,
        evidence.clipped.extended_frame_relative_to_outer,
        evidence.cleared.outer,
        evidence.cleared.client,
        evidence.cleared.extended_frame_relative_to_outer,
        evidence.installed_clip_bounds,
        evidence.queried_clip_bounds,
    ];
    if measurements.iter().any(|bounds| !bounds.is_valid()) {
        return MechanicalVerdict::Fail(
            "a required outer, client, frame, or region measurement was invalid".to_owned(),
        );
    }
    if evidence.hidden_baseline
        != (OuterClientBounds {
            outer: evidence.composed_baseline.outer,
            client: evidence.composed_baseline.client,
        })
    {
        return MechanicalVerdict::Fail(
            "showing the fixture changed its native outer or client dimensions".to_owned(),
        );
    }
    if evidence.clipped.outer != evidence.composed_baseline.outer
        || evidence.clipped.client != evidence.composed_baseline.client
    {
        return MechanicalVerdict::Fail(
            "clipping changed the fixture's native outer or client dimensions".to_owned(),
        );
    }
    if evidence.cleared.outer != evidence.composed_baseline.outer
        || evidence.cleared.client != evidence.composed_baseline.client
        || evidence.cleared.extended_frame_relative_to_outer
            != evidence.composed_baseline.extended_frame_relative_to_outer
        || evidence.cleared.nc_rendering_enabled != evidence.composed_baseline.nc_rendering_enabled
    {
        return MechanicalVerdict::Fail(
            "clearing did not restore native dimensions, relative frame bounds, or non-client rendering"
                .to_owned(),
        );
    }
    if evidence.queried_clip_bounds != evidence.installed_clip_bounds {
        return MechanicalVerdict::Fail(
            "the installed clipped region did not read back with its requested bounds".to_owned(),
        );
    }

    let mut presentation_limitations = Vec::new();
    if evidence.clipped.nc_rendering_enabled != evidence.composed_baseline.nc_rendering_enabled {
        presentation_limitations.push("non-client rendering changed while clipped");
    }
    if evidence.clipped.extended_frame_relative_to_outer
        != evidence.composed_baseline.extended_frame_relative_to_outer
    {
        presentation_limitations.push("extended frame bounds changed while clipped");
    }
    MechanicalVerdict::Pass {
        presentation_limitations,
    }
}

struct TestDpiContext(DPI_AWARENESS_CONTEXT);

impl TestDpiContext {
    unsafe fn enter() -> Result<Self, String> {
        let previous = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE);
        if previous.0.is_null() {
            return Err(format!(
                "INCONCLUSIVE: failed to establish physical test coordinates: {}",
                windows::core::Error::from_thread()
            ));
        }
        Ok(Self(previous))
    }
}

impl Drop for TestDpiContext {
    fn drop(&mut self) {
        unsafe {
            SetThreadDpiAwarenessContext(self.0);
        }
    }
}

unsafe fn current_process_creation_filetime() -> Result<u64, String> {
    let mut creation = FILETIME::default();
    let mut unused = FILETIME::default();
    GetProcessTimes(
        GetCurrentProcess(),
        &mut creation,
        &mut unused,
        &mut unused,
        &mut unused,
    )
    .map_err(|error| {
        format!("INCONCLUSIVE: failed to read fixture process creation time: {error}")
    })?;
    let value = ((creation.dwHighDateTime as u64) << 32) | u64::from(creation.dwLowDateTime);
    if value == 0 {
        return Err("INCONCLUSIVE: fixture process creation time was zero".to_owned());
    }
    Ok(value)
}

struct RegisteredClass {
    name: Vec<u16>,
}

impl RegisteredClass {
    unsafe fn register(name: Vec<u16>) -> Result<Self, String> {
        let class = WNDCLASSW {
            lpfnWndProc: Some(fixture_window_proc),
            lpszClassName: PCWSTR(name.as_ptr()),
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            return Err(format!(
                "INCONCLUSIVE: failed to register owned fixture class: {}",
                windows::core::Error::from_thread()
            ));
        }
        Ok(Self { name })
    }
}

impl Drop for RegisteredClass {
    fn drop(&mut self) {
        unsafe {
            let _ = UnregisterClassW(PCWSTR(self.name.as_ptr()), None);
        }
    }
}

struct OwnedFixtureWindow {
    hwnd: HWND,
    region_installed: bool,
    visible_since: Option<Instant>,
}

impl OwnedFixtureWindow {
    unsafe fn create(class_name: PCWSTR, title: PCWSTR) -> Result<Self, String> {
        Self::create_with_bounds(
            class_name,
            title,
            24,
            24,
            360,
            240,
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        )
    }

    unsafe fn create_with_bounds(
        class_name: PCWSTR,
        title: PCWSTR,
        left: i32,
        top: i32,
        width: i32,
        height: i32,
        ex_style: windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE,
    ) -> Result<Self, String> {
        let hwnd = CreateWindowExW(
            ex_style,
            class_name,
            title,
            WS_OVERLAPPEDWINDOW,
            left,
            top,
            width,
            height,
            None,
            None,
            None,
            None,
        )
        .map_err(|error| format!("INCONCLUSIVE: failed to create owned fixture: {error}"))?;
        Ok(Self {
            hwnd,
            region_installed: false,
            visible_since: None,
        })
    }

    unsafe fn clear_region(&mut self) -> Result<(), String> {
        let result = SetWindowRgn(self.hwnd, None, true);
        eprintln!(
            "framed-clipping-fixture hwnd={:?} state=clear-known-absent-region raw-set-window-rgn-result={result}",
            self.hwnd
        );
        if result == 0 {
            return Err(format!(
                "MECHANICAL FAIL: failed to clear the owned fixture region: {}",
                windows::core::Error::from_thread()
            ));
        }
        self.region_installed = false;
        Ok(())
    }

    unsafe fn cleanup(&mut self) -> Result<Option<Duration>, String> {
        if self.hwnd.is_invalid() {
            return Ok(None);
        }
        let mut failures = Vec::new();
        if self.region_installed {
            if let Err(error) = self.clear_region() {
                failures.push(error);
            }
        }
        let visible_elapsed = self.visible_since.map(|started| started.elapsed());
        let _ = ShowWindow(self.hwnd, SW_HIDE);
        if IsWindowVisible(self.hwnd).as_bool() {
            failures.push("failed to hide owned fixture during cleanup".to_owned());
        }
        if let Err(error) = DestroyWindow(self.hwnd) {
            failures.push(format!("failed to destroy owned fixture: {error}"));
        } else if IsWindow(Some(self.hwnd)).as_bool() {
            failures.push("owned fixture remained live after destruction".to_owned());
        }
        self.hwnd = HWND::default();
        if failures.is_empty() {
            Ok(visible_elapsed)
        } else {
            Err(format!(
                "MECHANICAL FAIL: fixture cleanup: {}",
                failures.join("; ")
            ))
        }
    }
}

impl Drop for OwnedFixtureWindow {
    fn drop(&mut self) {
        if self.hwnd.is_invalid() {
            return;
        }
        unsafe {
            if self.region_installed {
                let _ = SetWindowRgn(self.hwnd, None, true);
            }
            let _ = ShowWindow(self.hwnd, SW_HIDE);
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

unsafe extern "system" fn fixture_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    DefWindowProcW(hwnd, message, wparam, lparam)
}

unsafe fn outer_client_bounds(
    hwnd: HWND,
    process_id: u32,
    creation_filetime: u64,
    state: &str,
) -> Result<OuterClientBounds, String> {
    let mut outer = RECT::default();
    GetWindowRect(hwnd, &mut outer)
        .map_err(|error| format!("INCONCLUSIVE: GetWindowRect failed: {error}"))?;
    let outer = Bounds::from(outer);
    eprintln!(
        "framed-clipping-fixture pid={process_id} creation_filetime={creation_filetime} hwnd={hwnd:?} state={state} outer={outer:?}"
    );
    let mut client = RECT::default();
    GetClientRect(hwnd, &mut client)
        .map_err(|error| format!("INCONCLUSIVE: GetClientRect failed: {error}"))?;
    let client = Bounds::from(client);
    eprintln!(
        "framed-clipping-fixture pid={process_id} creation_filetime={creation_filetime} hwnd={hwnd:?} state={state} client={client:?}"
    );
    Ok(OuterClientBounds { outer, client })
}

unsafe fn composed_measurement(
    hwnd: HWND,
    process_id: u32,
    creation_filetime: u64,
    state: &str,
) -> Result<ComposedMeasurement, String> {
    let outer_client = outer_client_bounds(hwnd, process_id, creation_filetime, state)?;
    let mut nc_rendering_enabled = BOOL::default();
    DwmGetWindowAttribute(
        hwnd,
        DWMWA_NCRENDERING_ENABLED,
        &mut nc_rendering_enabled as *mut BOOL as *mut c_void,
        std::mem::size_of::<BOOL>() as u32,
    )
    .map_err(|error| format!("INCONCLUSIVE: DWMWA_NCRENDERING_ENABLED was unreadable: {error}"))?;
    let nc_rendering_enabled = nc_rendering_enabled.as_bool();
    eprintln!(
        "framed-clipping-fixture pid={process_id} creation_filetime={creation_filetime} hwnd={hwnd:?} state={state} nc_rendering_enabled={nc_rendering_enabled}"
    );
    let mut extended_frame = RECT::default();
    DwmGetWindowAttribute(
        hwnd,
        DWMWA_EXTENDED_FRAME_BOUNDS,
        &mut extended_frame as *mut RECT as *mut c_void,
        std::mem::size_of::<RECT>() as u32,
    )
    .map_err(|error| {
        format!("INCONCLUSIVE: DWMWA_EXTENDED_FRAME_BOUNDS was unreadable: {error}")
    })?;
    let extended_frame_relative_to_outer =
        Bounds::from(extended_frame).relative_to(outer_client.outer);
    eprintln!(
        "framed-clipping-fixture pid={process_id} creation_filetime={creation_filetime} hwnd={hwnd:?} state={state} relative_frame={extended_frame_relative_to_outer:?}"
    );
    Ok(ComposedMeasurement {
        outer: outer_client.outer,
        client: outer_client.client,
        extended_frame_relative_to_outer,
        nc_rendering_enabled,
    })
}

unsafe fn flush_composition() -> Result<(), String> {
    DwmFlush().map_err(|error| format!("INCONCLUSIVE: DwmFlush failed: {error}"))
}

unsafe fn assert_daemon_excluded_fixture(hwnd: HWND) -> Result<(), String> {
    let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
    let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
    if !crate::enumeration::is_excluded_tool_window(style, ex_style) {
        return Err(
            "INCONCLUSIVE: owned fixture is not excluded by the tool-window admission rule"
                .to_owned(),
        );
    }
    if ex_style & WS_EX_NOACTIVATE.0 == 0 {
        return Err("INCONCLUSIVE: owned fixture lost WS_EX_NOACTIVATE".to_owned());
    }
    if crate::enumeration::should_emit_window_event_with_policy(hwnd, false, false, false) {
        return Err(
            "INCONCLUSIVE: owned fixture passed daemon event admission before show".to_owned(),
        );
    }
    Ok(())
}

unsafe fn install_half_width_clip(
    fixture: &mut OwnedFixtureWindow,
    full_outer: Bounds,
    process_id: u32,
    creation_filetime: u64,
) -> Result<Bounds, String> {
    let clip = Bounds {
        left: 0,
        top: 0,
        right: full_outer.width() / 2,
        bottom: full_outer.height(),
    };
    if !clip.is_valid() {
        return Err("INCONCLUSIVE: fixture was too small for a half-width clip".to_owned());
    }
    eprintln!(
        "framed-clipping-fixture pid={process_id} creation_filetime={creation_filetime} hwnd={:?} state=clipped-region-requested bounds={clip:?}",
        fixture.hwnd
    );
    let region = CreateRectRgn(clip.left, clip.top, clip.right, clip.bottom);
    if region.is_invalid() {
        return Err(format!(
            "INCONCLUSIVE: failed to create owned clipping region: {}",
            windows::core::Error::from_thread()
        ));
    }
    if SetWindowRgn(fixture.hwnd, Some(region), true) == 0 {
        let _ = DeleteObject(region.into());
        return Err(format!(
            "MECHANICAL FAIL: failed to install owned clipping region: {}",
            windows::core::Error::from_thread()
        ));
    }
    fixture.region_installed = true;
    Ok(clip)
}

unsafe fn queried_clip_bounds(
    hwnd: HWND,
    process_id: u32,
    creation_filetime: u64,
) -> Result<Bounds, String> {
    let mut bounds = RECT::default();
    let result = GetWindowRgnBox(hwnd, &mut bounds);
    let bounds = Bounds::from(bounds);
    eprintln!(
        "framed-clipping-fixture pid={process_id} creation_filetime={creation_filetime} hwnd={hwnd:?} state=clipped-region-readback raw-region-type={} bounds={bounds:?}",
        result.0
    );
    if result.0 == ERROR {
        return Err("INCONCLUSIVE: installed clipping region could not be read".to_owned());
    }
    Ok(bounds)
}

unsafe fn run_fixture_message_thread() -> Result<MechanicalVerdict, String> {
    let _dpi = TestDpiContext::enter()?;
    let process_id = std::process::id();
    let creation_filetime = current_process_creation_filetime()?;
    let class_name: Vec<u16> =
        format!("LeopardWMFramedClipProof-{process_id}-{creation_filetime}\0")
            .encode_utf16()
            .collect();
    let title: Vec<u16> =
        format!("LeopardWM framed clipping proof {process_id} {creation_filetime}\0")
            .encode_utf16()
            .collect();
    let class = RegisteredClass::register(class_name)?;
    let mut fixture =
        OwnedFixtureWindow::create(PCWSTR(class.name.as_ptr()), PCWSTR(title.as_ptr()))?;
    eprintln!(
        "framed-clipping-fixture pid={process_id} creation_filetime={creation_filetime} hwnd={:?} state=created-hidden original-region=known-absent-at-fixture-creation",
        fixture.hwnd
    );

    let proof = (|| {
        assert_daemon_excluded_fixture(fixture.hwnd)?;
        if IsWindowVisible(fixture.hwnd).as_bool() {
            return Err(
                "INCONCLUSIVE: owned fixture was visible before the safety gate".to_owned(),
            );
        }
        let hidden_baseline = outer_client_bounds(
            fixture.hwnd,
            process_id,
            creation_filetime,
            "hidden-baseline",
        )?;

        let _ = ShowWindow(fixture.hwnd, SW_SHOWNOACTIVATE);
        eprintln!(
            "framed-clipping-fixture pid={process_id} creation_filetime={creation_filetime} hwnd={:?} state=shown-noactivate",
            fixture.hwnd
        );
        if !IsWindowVisible(fixture.hwnd).as_bool() {
            return Err("INCONCLUSIVE: owned fixture did not become visible".to_owned());
        }
        fixture.visible_since = Some(Instant::now());
        flush_composition()?;
        let composed_baseline = composed_measurement(
            fixture.hwnd,
            process_id,
            creation_filetime,
            "visible-composed-baseline",
        )?;

        let installed_clip_bounds = install_half_width_clip(
            &mut fixture,
            composed_baseline.outer,
            process_id,
            creation_filetime,
        )?;
        flush_composition()?;
        let clipped = composed_measurement(
            fixture.hwnd,
            process_id,
            creation_filetime,
            "visible-clipped",
        )?;
        let queried_clip_bounds = queried_clip_bounds(fixture.hwnd, process_id, creation_filetime)?;

        fixture.clear_region()?;
        flush_composition()?;
        let cleared = composed_measurement(
            fixture.hwnd,
            process_id,
            creation_filetime,
            "visible-cleared",
        )?;

        Ok(evaluate(&FramedWindowEvidence {
            hidden_baseline,
            composed_baseline,
            clipped,
            cleared,
            installed_clip_bounds,
            queried_clip_bounds,
        }))
    })();

    let cleanup = fixture.cleanup();
    match (proof, cleanup) {
        (Ok(verdict), Ok(visible_elapsed)) => {
            eprintln!(
                "framed-clipping-fixture pid={process_id} creation_filetime={creation_filetime} state=hidden-destroyed visible-elapsed-ms={:?}",
                visible_elapsed.map(|elapsed| elapsed.as_millis())
            );
            Ok(verdict)
        }
        (Err(proof_error), Ok(visible_elapsed)) => {
            eprintln!(
                "framed-clipping-fixture pid={process_id} creation_filetime={creation_filetime} state=hidden-destroyed visible-elapsed-ms={:?}",
                visible_elapsed.map(|elapsed| elapsed.as_millis())
            );
            Err(proof_error)
        }
        (Ok(_), Err(cleanup_error)) => Err(cleanup_error),
        (Err(proof_error), Err(cleanup_error)) => {
            Err(format!("{proof_error}; fixture cleanup: {cleanup_error}"))
        }
    }
}

fn run_child_fixture() -> Result<MechanicalVerdict, String> {
    let (stop_tx, stop_rx) = mpsc::channel();
    let watchdog = thread::Builder::new()
        .name("framed-window-proof-deadline".to_owned())
        .spawn(move || {
            if stop_rx.recv_timeout(FIXTURE_DEADLINE).is_err() {
                eprintln!("framed-clipping-fixture state=deadline-process-exit");
                std::process::exit(124);
            }
        })
        .map_err(|error| {
            format!("INCONCLUSIVE: failed to start fixture deadline watchdog: {error}")
        })?;
    let result = thread::Builder::new()
        .name("framed-window-proof-fixture-message-thread".to_owned())
        .spawn(|| unsafe { run_fixture_message_thread() })
        .map_err(|error| format!("INCONCLUSIVE: failed to start fixture thread: {error}"))?
        .join()
        .map_err(|_| "INCONCLUSIVE: fixture message thread panicked".to_owned())?;
    let _ = stop_tx.send(());
    let _ = watchdog.join();
    result
}

fn wait_for_supervised_child(child: &mut Child) -> Result<ExitStatus, String> {
    let deadline = Instant::now() + SUPERVISOR_DEADLINE;
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("INCONCLUSIVE: failed to wait for fixture child: {error}"))?
        {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            child.kill().map_err(|error| {
                format!("INCONCLUSIVE: failed to terminate retained fixture child: {error}")
            })?;
            let _ = child.wait();
            return Err(
                "INCONCLUSIVE: retained fixture child exceeded its 13-second supervisor deadline"
                    .to_owned(),
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum KnownRegionState {
    Absent,
    Empty,
    Simple,
    Complex,
}

impl KnownRegionState {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "absent" => Ok(Self::Absent),
            "empty" => Ok(Self::Empty),
            "simple" => Ok(Self::Simple),
            "complex" => Ok(Self::Complex),
            _ => Err(format!("INCONCLUSIVE: unknown matrix region state {value}")),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Empty => "empty",
            Self::Simple => "simple",
            Self::Complex => "complex",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SerializedRegion {
    Absent,
    Data(Vec<u8>),
}

struct AlignedRegionData {
    words: Vec<u32>,
    byte_len: usize,
}

impl AlignedRegionData {
    fn zeroed(byte_len: usize) -> Result<Self, String> {
        let word_len = byte_len
            .checked_add(std::mem::size_of::<u32>() - 1)
            .ok_or_else(|| {
                "INCONCLUSIVE: region data length overflowed alignment storage".to_owned()
            })?
            / std::mem::size_of::<u32>();
        Ok(Self {
            words: vec![0; word_len],
            byte_len,
        })
    }

    fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let mut data = Self::zeroed(bytes.len())?;
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                data.words.as_mut_ptr().cast::<u8>(),
                bytes.len(),
            );
        }
        Ok(data)
    }

    fn byte_len_u32(&self) -> Result<u32, String> {
        u32::try_from(self.byte_len)
            .map_err(|_| "INCONCLUSIVE: region data length exceeded Win32 limits".to_owned())
    }

    fn as_rgndata(&self) -> *const RGNDATA {
        self.words.as_ptr().cast()
    }

    fn as_mut_rgndata(&mut self) -> *mut RGNDATA {
        self.words.as_mut_ptr().cast()
    }

    fn into_bytes(self) -> Vec<u8> {
        unsafe {
            std::slice::from_raw_parts(self.words.as_ptr().cast::<u8>(), self.byte_len).to_vec()
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MatrixLayout {
    Ltr,
    Rtl,
}

impl MatrixLayout {
    fn parse(value: Option<String>) -> Result<Self, String> {
        match value.as_deref().unwrap_or("ltr") {
            "ltr" => Ok(Self::Ltr),
            "rtl" => Ok(Self::Rtl),
            value => Err(format!("INCONCLUSIVE: unknown matrix layout {value}")),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Ltr => "ltr",
            Self::Rtl => "rtl",
        }
    }

    fn ex_style(self) -> windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE {
        let base = WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
        match self {
            Self::Ltr => base,
            Self::Rtl => base | WS_EX_LAYOUTRTL,
        }
    }

    fn allowed_slice(self) -> Bounds {
        self.region_for_screen_slice(Bounds {
            left: 0,
            top: 0,
            right: 800,
            bottom: 1430,
        })
    }

    fn region_for_screen_slice(self, screen_slice: Bounds) -> Bounds {
        match self {
            Self::Ltr => screen_slice,
            // Mirrored windows express regions from the native right origin. This maps the
            // screen-left 800px of the 1600px frame at x=4320 to [800, 1600].
            Self::Rtl => Bounds {
                left: 1600 - screen_slice.right,
                top: screen_slice.top,
                right: 1600 - screen_slice.left,
                bottom: screen_slice.bottom,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FixtureIdentity {
    hwnd: HWND,
    pid: u32,
    creation_filetime: u64,
    generation: usize,
}

fn property_name() -> Vec<u16> {
    format!("{MATRIX_GENERATION_PROPERTY}\0")
        .encode_utf16()
        .collect()
}

unsafe fn process_creation_filetime(pid: u32) -> Result<u64, String> {
    let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
        .map_err(|error| format!("INCONCLUSIVE: OpenProcess({pid}) failed: {error}"))?;
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let result = GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user);
    let _ = CloseHandle(process);
    result.map_err(|error| format!("INCONCLUSIVE: GetProcessTimes({pid}) failed: {error}"))?;
    let value = ((creation.dwHighDateTime as u64) << 32) | u64::from(creation.dwLowDateTime);
    if value == 0 {
        return Err(format!(
            "INCONCLUSIVE: process {pid} creation time was zero"
        ));
    }
    Ok(value)
}

unsafe fn create_known_region(state: KnownRegionState) -> Result<HRGN, String> {
    let first = match state {
        KnownRegionState::Empty => CreateRectRgn(0, 0, 0, 0),
        KnownRegionState::Simple | KnownRegionState::Complex => CreateRectRgn(12, 16, 300, 180),
        KnownRegionState::Absent => {
            return Err("INCONCLUSIVE: absent has no region handle".to_owned())
        }
    };
    if first.is_invalid() {
        return Err("INCONCLUSIVE: could not create known fixture region".to_owned());
    }
    if state == KnownRegionState::Complex {
        let second = CreateRectRgn(180, 80, 340, 220);
        if second.is_invalid() {
            let _ = DeleteObject(first.into());
            return Err("INCONCLUSIVE: could not create complex fixture region".to_owned());
        }
        let combined = CombineRgn(Some(first), Some(first), Some(second), RGN_OR);
        let _ = DeleteObject(second.into());
        if combined.0 == ERROR {
            let _ = DeleteObject(first.into());
            return Err("MECHANICAL FAIL: could not combine complex fixture region".to_owned());
        }
    }
    Ok(first)
}

unsafe fn install_known_region(hwnd: HWND, state: KnownRegionState) -> Result<(), String> {
    if state == KnownRegionState::Absent {
        if SetWindowRgn(hwnd, None, true) == 0 {
            return Err(
                "MECHANICAL FAIL: could not establish known absent fixture region".to_owned(),
            );
        }
        eprintln!("clipping-matrix-fixture hwnd={hwnd:?} state=known-absent-established-by-explicit-clear");
        return Ok(());
    }
    let region = create_known_region(state)?;
    if SetWindowRgn(hwnd, Some(region), true) == 0 {
        let _ = DeleteObject(region.into());
        return Err("MECHANICAL FAIL: could not install known fixture region".to_owned());
    }
    Ok(())
}

unsafe fn serialize_region(region: HRGN) -> Result<SerializedRegion, String> {
    let required = GetRegionData(region, 0, None);
    if required == 0 {
        return Err("INCONCLUSIVE: region serialization length was unavailable".to_owned());
    }
    let mut data = AlignedRegionData::zeroed(required as usize)?;
    let copied = GetRegionData(region, required, Some(data.as_mut_rgndata()));
    if copied != required {
        return Err("INCONCLUSIVE: region serialization was incomplete".to_owned());
    }
    Ok(SerializedRegion::Data(data.into_bytes()))
}

unsafe fn serialize_installed_region(hwnd: HWND) -> Result<SerializedRegion, String> {
    let region = CreateRectRgn(0, 0, 0, 0);
    if region.is_invalid() {
        return Err("INCONCLUSIVE: could not allocate region serialization buffer".to_owned());
    }
    let region_type = GetWindowRgn(hwnd, region);
    if region_type.0 == ERROR {
        let _ = DeleteObject(region.into());
        return Err("INCONCLUSIVE: region query failed; unknown state is not mutated".to_owned());
    }
    let serialized = serialize_region(region);
    let _ = DeleteObject(region.into());
    serialized
}

unsafe fn expected_owned_region(
    state: KnownRegionState,
    outer: Bounds,
    allowed_region: Option<Bounds>,
) -> Result<SerializedRegion, String> {
    if outer.width() != 1600 || outer.height() != 1440 {
        return Err(
            "INCONCLUSIVE: matrix fixture did not retain the required 1600x1440 native frame"
                .to_owned(),
        );
    }
    let clip = match allowed_region {
        Some(slice) => CreateRectRgn(slice.left, slice.top, slice.right, slice.bottom),
        None => CreateRectRgn(0, 0, 0, 0),
    };
    if clip.is_invalid() {
        return Err("INCONCLUSIVE: could not create matrix allowed slice".to_owned());
    }
    if state == KnownRegionState::Absent {
        let serialized = serialize_region(clip);
        let _ = DeleteObject(clip.into());
        return serialized;
    }
    let original = match create_known_region(state) {
        Ok(original) => original,
        Err(error) => {
            let _ = DeleteObject(clip.into());
            return Err(error);
        }
    };
    let combined = CombineRgn(Some(original), Some(original), Some(clip), RGN_AND);
    let _ = DeleteObject(clip.into());
    if combined.0 == ERROR {
        let _ = DeleteObject(original.into());
        return Err(
            "MECHANICAL FAIL: could not intersect known original and allowed slice".to_owned(),
        );
    }
    let serialized = serialize_region(original);
    let _ = DeleteObject(original.into());
    serialized
}

unsafe fn expected_owned_clip(
    state: KnownRegionState,
    outer: Bounds,
    layout: MatrixLayout,
) -> Result<SerializedRegion, String> {
    expected_owned_region(state, outer, Some(layout.allowed_slice()))
}

unsafe fn application_replacement_region() -> Result<SerializedRegion, String> {
    let region = CreateRectRgn(240, 40, 640, 500);
    if region.is_invalid() {
        return Err("INCONCLUSIVE: could not create application replacement region".to_owned());
    }
    let serialized = serialize_region(region);
    let _ = DeleteObject(region.into());
    serialized
}

unsafe fn install_serialized_region(hwnd: HWND, region: &SerializedRegion) -> Result<(), String> {
    let SerializedRegion::Data(bytes) = region else {
        return Err("INCONCLUSIVE: application replacement was unexpectedly absent".to_owned());
    };
    let data = AlignedRegionData::from_bytes(bytes)?;
    let handle = ExtCreateRegion(None, data.byte_len_u32()?, data.as_rgndata());
    if handle.is_invalid() {
        return Err(
            "INCONCLUSIVE: could not reconstruct application replacement region".to_owned(),
        );
    }
    if SetWindowRgn(hwnd, Some(handle), true) == 0 {
        let _ = DeleteObject(handle.into());
        return Err("MECHANICAL FAIL: could not install application replacement region".to_owned());
    }
    Ok(())
}

unsafe fn serialized_known_region_shape(
    state: KnownRegionState,
) -> Result<SerializedRegion, String> {
    if state == KnownRegionState::Absent {
        return Ok(SerializedRegion::Absent);
    }
    let region = create_known_region(state)?;
    let serialized = serialize_region(region);
    let _ = DeleteObject(region.into());
    serialized
}

unsafe fn capture_known_original(
    hwnd: HWND,
    state: KnownRegionState,
) -> Result<SerializedRegion, String> {
    if state == KnownRegionState::Absent {
        return Ok(SerializedRegion::Absent);
    }
    serialize_installed_region(hwnd)
}

unsafe fn restore_serialized_region(hwnd: HWND, region: &SerializedRegion) -> Result<(), String> {
    restore_serialized_region_injected(hwnd, region, &mut RestoreInjection::None)
}

unsafe fn restore_serialized_region_injected(
    hwnd: HWND,
    region: &SerializedRegion,
    injection: &mut RestoreInjection,
) -> Result<(), String> {
    if injection.should_fail() {
        return Err("INJECTED: restore failed before SetWindowRgn".to_owned());
    }
    match region {
        SerializedRegion::Absent => {
            if SetWindowRgn(hwnd, None, true) == 0 {
                return Err("MECHANICAL FAIL: failed to restore known absent region".to_owned());
            }
        }
        SerializedRegion::Data(bytes) => {
            let data = AlignedRegionData::from_bytes(bytes)?;
            let restored = ExtCreateRegion(None, data.byte_len_u32()?, data.as_rgndata());
            if restored.is_invalid() {
                return Err("MECHANICAL FAIL: failed to reconstruct serialized region".to_owned());
            }
            if SetWindowRgn(hwnd, Some(restored), true) == 0 {
                let _ = DeleteObject(restored.into());
                return Err("MECHANICAL FAIL: failed to restore serialized region".to_owned());
            }
        }
    }
    Ok(())
}

unsafe fn verify_fixture_identity(identity: FixtureIdentity) -> Result<(), String> {
    if !IsWindow(Some(identity.hwnd)).as_bool() {
        return Err("INCONCLUSIVE: fixture HWND is no longer live".to_owned());
    }
    let mut pid = 0;
    GetWindowThreadProcessId(identity.hwnd, Some(&mut pid));
    if pid != identity.pid || process_creation_filetime(pid)? != identity.creation_filetime {
        return Err("INCONCLUSIVE: fixture PID/creation identity changed".to_owned());
    }
    let property = property_name();
    let generation = GetPropW(identity.hwnd, PCWSTR(property.as_ptr()));
    if generation.0 as usize != identity.generation {
        return Err("INCONCLUSIVE: fixture generation property changed".to_owned());
    }
    Ok(())
}

fn synthetic_visible_slice(frame: Bounds, viewport: Bounds) -> Option<Bounds> {
    let left = frame.left.max(viewport.left);
    let top = frame.top.max(viewport.top);
    let right = frame.right.min(viewport.right);
    let bottom = frame.bottom.min(viewport.bottom);
    (right > left && bottom > top).then_some(Bounds {
        left: left - frame.left,
        top: top - frame.top,
        right: right - frame.left,
        bottom: bottom - frame.top,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MatrixBoundary {
    Complete,
    BeforeMark,
    Marked,
    Clipped,
    BeforeRestore,
    RestoredBeforeMarkerRemoval,
    MarkerRemoved,
    ApplicationReplacementBeforeMarkerRemoval,
    ApplicationReplacementAfterMarkerRemoval,
}

impl MatrixBoundary {
    fn parse(value: Option<String>) -> Result<Self, String> {
        match value.as_deref().unwrap_or("complete") {
            "complete" => Ok(Self::Complete),
            "before-mark" => Ok(Self::BeforeMark),
            "marked" => Ok(Self::Marked),
            "clipped" => Ok(Self::Clipped),
            "before-restore" => Ok(Self::BeforeRestore),
            "restored-before-marker-removal" => Ok(Self::RestoredBeforeMarkerRemoval),
            "marker-removed" => Ok(Self::MarkerRemoved),
            "application-replacement-before-marker-removal" => {
                Ok(Self::ApplicationReplacementBeforeMarkerRemoval)
            }
            "application-replacement-after-marker-removal" => {
                Ok(Self::ApplicationReplacementAfterMarkerRemoval)
            }
            other => Err(format!("INCONCLUSIVE: unknown matrix boundary {other}")),
        }
    }

    fn phase(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::BeforeMark => "before-mark",
            Self::Marked => "marked",
            Self::Clipped => "clipped",
            Self::BeforeRestore => "before-restore",
            Self::RestoredBeforeMarkerRemoval => "restored-before-marker-removal",
            Self::MarkerRemoved => "marker-removed",
            Self::ApplicationReplacementBeforeMarkerRemoval => {
                "application-replacement-before-marker-removal"
            }
            Self::ApplicationReplacementAfterMarkerRemoval => {
                "application-replacement-after-marker-removal"
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MatrixFault {
    Query,
    Placement,
    Install,
    RestoreOnce,
    RestoreTerminal,
}

impl MatrixFault {
    fn parse(value: Option<String>) -> Result<Option<Self>, String> {
        match value.as_deref() {
            None => Ok(None),
            Some("query") => Ok(Some(Self::Query)),
            Some("placement") => Ok(Some(Self::Placement)),
            Some("install") => Ok(Some(Self::Install)),
            Some("restore-once") => Ok(Some(Self::RestoreOnce)),
            Some("restore-terminal") => Ok(Some(Self::RestoreTerminal)),
            Some(value) => Err(format!(
                "INCONCLUSIVE: unknown matrix injected fault {value}"
            )),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::Placement => "placement",
            Self::Install => "install",
            Self::RestoreOnce => "restore-once",
            Self::RestoreTerminal => "restore-terminal",
        }
    }

    fn is_controller_fault(self) -> bool {
        matches!(self, Self::Query | Self::Placement | Self::Install)
    }
}

#[derive(Clone, Debug)]
struct RecoveryRecord {
    identity: FixtureIdentity,
    original: SerializedRegion,
    expected_installed: SerializedRegion,
    dimensions: OuterClientBounds,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RestoreInjection {
    None,
    FailOnce,
    FailAlways,
}

impl RestoreInjection {
    fn from_fault(fault: Option<MatrixFault>) -> Self {
        match fault {
            Some(MatrixFault::RestoreOnce) => Self::FailOnce,
            Some(MatrixFault::RestoreTerminal) => Self::FailAlways,
            _ => Self::None,
        }
    }

    fn should_fail(&mut self) -> bool {
        match self {
            Self::None => false,
            Self::FailOnce => {
                *self = Self::None;
                true
            }
            Self::FailAlways => true,
        }
    }
}

fn matrix_property_name(name: &str) -> Vec<u16> {
    format!("{name}\0").encode_utf16().collect()
}

unsafe fn set_generation_property(hwnd: HWND, generation: usize) -> Result<(), String> {
    let property = property_name();
    SetPropW(
        hwnd,
        PCWSTR(property.as_ptr()),
        Some(HANDLE(generation as *mut c_void)),
    )
    .map_err(|error| format!("INCONCLUSIVE: could not set fixture generation property: {error}"))
}

unsafe fn set_ownership_property(identity: FixtureIdentity) -> Result<(), String> {
    let property = matrix_property_name(MATRIX_OWNERSHIP_PROPERTY);
    SetPropW(
        identity.hwnd,
        PCWSTR(property.as_ptr()),
        Some(HANDLE(identity.generation as *mut c_void)),
    )
    .map_err(|error| format!("MECHANICAL FAIL: could not mark owned clipping region: {error}"))
}

unsafe fn clear_ownership_property(hwnd: HWND) {
    let property = matrix_property_name(MATRIX_OWNERSHIP_PROPERTY);
    let _ = RemovePropW(hwnd, PCWSTR(property.as_ptr()));
}

unsafe fn ownership_matches(identity: FixtureIdentity) -> bool {
    let property = matrix_property_name(MATRIX_OWNERSHIP_PROPERTY);
    GetPropW(identity.hwnd, PCWSTR(property.as_ptr())).0 as usize == identity.generation
}

unsafe fn assert_matrix_fixture_geometry(
    identity: FixtureIdentity,
    layout: MatrixLayout,
) -> Result<OuterClientBounds, String> {
    let measured = outer_client_bounds(
        identity.hwnd,
        identity.pid,
        identity.creation_filetime,
        "matrix-required-hidden-frame",
    )?;
    let expected_outer = Bounds {
        left: 4320,
        top: 10,
        right: 5920,
        bottom: 1450,
    };
    if measured.outer != expected_outer {
        return Err(format!(
            "INCONCLUSIVE: hidden matrix fixture outer bounds were {:?}, expected {:?}",
            measured.outer, expected_outer
        ));
    }
    let ex_style = GetWindowLongW(identity.hwnd, GWL_EXSTYLE) as u32;
    let rtl = ex_style & WS_EX_LAYOUTRTL.0 != 0;
    if rtl != (layout == MatrixLayout::Rtl) {
        return Err(
            "INCONCLUSIVE: hidden matrix fixture layout style differed from requested layout"
                .to_owned(),
        );
    }
    Ok(measured)
}

unsafe fn install_owned_clip(
    identity: FixtureIdentity,
    state: KnownRegionState,
    layout: MatrixLayout,
    expected: &SerializedRegion,
) -> Result<OuterClientBounds, String> {
    let before = outer_client_bounds(
        identity.hwnd,
        identity.pid,
        identity.creation_filetime,
        "matrix-before-clip",
    )?;
    if !before.outer.is_valid() || !before.client.is_valid() {
        return Err(
            "MECHANICAL FAIL: matrix fixture dimensions were invalid before clipping".to_owned(),
        );
    }
    let controller_expected = expected_owned_clip(state, before.outer, layout)?;
    if &controller_expected != expected {
        return Err(
            "INCONCLUSIVE: controller allowed slice disagreed with supervisor expected state"
                .to_owned(),
        );
    }
    let SerializedRegion::Data(bytes) = expected else {
        return Err("INCONCLUSIVE: owned clipping region was unexpectedly absent".to_owned());
    };
    eprintln!(
        "clipping-matrix-controller hwnd={:?} state=allowed-slice-requested layout={} bounds={:?}",
        identity.hwnd,
        layout.name(),
        layout.allowed_slice()
    );
    let data = AlignedRegionData::from_bytes(bytes)?;
    let clip = ExtCreateRegion(None, data.byte_len_u32()?, data.as_rgndata());
    if clip.is_invalid() {
        return Err("INCONCLUSIVE: could not reconstruct matrix clipping region".to_owned());
    }
    if SetWindowRgn(identity.hwnd, Some(clip), true) == 0 {
        let _ = DeleteObject(clip.into());
        return Err("MECHANICAL FAIL: could not install matrix clipping region".to_owned());
    }
    let after = outer_client_bounds(
        identity.hwnd,
        identity.pid,
        identity.creation_filetime,
        "matrix-after-clip",
    )?;
    if before != after {
        return Err(
            "MECHANICAL FAIL: matrix clipping changed native outer or client dimensions".to_owned(),
        );
    }
    let actual = serialize_installed_region(identity.hwnd)?;
    if actual != *expected {
        return Err(
            "MECHANICAL FAIL: installed matrix region did not match expected data".to_owned(),
        );
    }
    let mut region_bounds = RECT::default();
    let region_type = GetWindowRgnBox(identity.hwnd, &mut region_bounds);
    eprintln!(
        "clipping-matrix-controller hwnd={:?} state=installed-region-readback raw-region-type={} bounds={:?}",
        identity.hwnd,
        region_type.0,
        Bounds::from(region_bounds)
    );
    if state == KnownRegionState::Absent && Bounds::from(region_bounds) != layout.allowed_slice() {
        return Err(format!(
            "MECHANICAL FAIL: {} allowed slice did not read back as {:?}",
            layout.name(),
            layout.allowed_slice()
        ));
    }
    Ok(before)
}

unsafe fn install_progress_owned_region(
    identity: FixtureIdentity,
    expected: &SerializedRegion,
    expected_bounds: Option<Bounds>,
    state: &str,
) -> Result<(), String> {
    let before = outer_client_bounds(
        identity.hwnd,
        identity.pid,
        identity.creation_filetime,
        &format!("matrix-owned-progress-{state}-before-region"),
    )?;
    let SerializedRegion::Data(bytes) = expected else {
        return Err("INCONCLUSIVE: owned progress region was unexpectedly absent".to_owned());
    };
    let data = AlignedRegionData::from_bytes(bytes)?;
    let region = ExtCreateRegion(None, data.byte_len_u32()?, data.as_rgndata());
    if region.is_invalid() {
        return Err("INCONCLUSIVE: could not reconstruct owned progress region".to_owned());
    }
    if SetWindowRgn(identity.hwnd, Some(region), true) == 0 {
        let _ = DeleteObject(region.into());
        return Err(format!(
            "MECHANICAL FAIL: could not install owned progress region at {state}"
        ));
    }
    let after = outer_client_bounds(
        identity.hwnd,
        identity.pid,
        identity.creation_filetime,
        &format!("matrix-owned-progress-{state}-after-region"),
    )?;
    if before.outer.width() != after.outer.width()
        || before.outer.height() != after.outer.height()
        || before.client != after.client
    {
        return Err(format!(
            "MECHANICAL FAIL: owned progress region changed native dimensions at {state}"
        ));
    }
    if serialize_installed_region(identity.hwnd)? != *expected {
        return Err(format!(
            "MECHANICAL FAIL: owned progress region readback differed at {state}"
        ));
    }
    let mut bounds = RECT::default();
    let region_type = GetWindowRgnBox(identity.hwnd, &mut bounds);
    if region_type.0 == ERROR {
        return Err(format!(
            "INCONCLUSIVE: owned progress region bounds were unreadable at {state}"
        ));
    }
    let bounds = Bounds::from(bounds);
    eprintln!(
        "clipping-matrix-controller hwnd={:?} state=owned-progress-{state}-readback raw-region-type={} bounds={bounds:?}",
        identity.hwnd,
        region_type.0
    );
    if let Some(expected_bounds) = expected_bounds {
        if bounds != expected_bounds {
            return Err(format!(
                "MECHANICAL FAIL: owned progress region bounds differed at {state}"
            ));
        }
    }
    Ok(())
}

unsafe fn progress_hidden_matrix_geometry(
    identity: FixtureIdentity,
    baseline: OuterClientBounds,
    fault: Option<MatrixFault>,
) -> Result<(), String> {
    if baseline.outer.width() != 1600 || baseline.outer.height() != 1440 {
        return Err("INCONCLUSIVE: native matrix frame was not the required 1600x1440".to_owned());
    }
    if fault == Some(MatrixFault::Placement) {
        return Err("INJECTED: placement operation failed before SetWindowPos".to_owned());
    }
    let positions = [
        (3520, 10, "left"),
        (4320, -1430, "top"),
        (4320, 1430, "bottom"),
        (3520, 10, "reversal"),
        (4320, 10, "recenter"),
        (-32000, -32000, "parking"),
        (4320, 10, "parking-recenter"),
    ];
    for (left, top, state) in positions {
        SetWindowPos(
            identity.hwnd,
            None,
            left,
            top,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER,
        )
        .map_err(|error| {
            format!("MECHANICAL FAIL: hidden geometry {state} placement failed: {error}")
        })?;
        let measured = outer_client_bounds(
            identity.hwnd,
            identity.pid,
            identity.creation_filetime,
            &format!("matrix-hidden-{state}"),
        )?;
        if measured.outer.width() != baseline.outer.width()
            || measured.outer.height() != baseline.outer.height()
            || measured.client != baseline.client
        {
            return Err(format!(
                "MECHANICAL FAIL: hidden geometry {state} changed native dimensions"
            ));
        }
    }
    Ok(())
}

unsafe fn progress_owned_clip_geometry(
    identity: FixtureIdentity,
    state: KnownRegionState,
    layout: MatrixLayout,
    baseline: OuterClientBounds,
) -> Result<(), String> {
    if !ownership_matches(identity) {
        return Err("MECHANICAL FAIL: owned clip progression lost its ownership marker".to_owned());
    }
    let owner = Bounds {
        left: 0,
        top: 0,
        right: 5120,
        bottom: 1440,
    };
    let positions = [
        (-800, 10, "left-edge"),
        (4320, 10, "right-edge"),
        (3520, 10, "reversal-full"),
        (4320, -1430, "top-edge"),
        (4320, 1430, "bottom-edge"),
        (-32000, -32000, "parking"),
        (4320, 10, "recenter"),
    ];
    for (left, top, sample) in positions {
        SetWindowPos(
            identity.hwnd,
            None,
            left,
            top,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOZORDER,
        )
        .map_err(|error| {
            format!("MECHANICAL FAIL: owned clip progression {sample} placement failed: {error}")
        })?;
        let measured = outer_client_bounds(
            identity.hwnd,
            identity.pid,
            identity.creation_filetime,
            &format!("matrix-owned-progress-{sample}-placement"),
        )?;
        if measured.outer.width() != baseline.outer.width()
            || measured.outer.height() != baseline.outer.height()
            || measured.client != baseline.client
        {
            return Err(format!(
                "MECHANICAL FAIL: owned clip progression {sample} changed native dimensions"
            ));
        }
        let screen_slice = synthetic_visible_slice(
            Bounds {
                left,
                top,
                right: left + baseline.outer.width(),
                bottom: top + baseline.outer.height(),
            },
            owner,
        );
        let allowed_region = screen_slice.map(|slice| layout.region_for_screen_slice(slice));
        eprintln!(
            "clipping-matrix-controller hwnd={:?} state=owned-progress-{sample}-requested screen-slice={screen_slice:?} region={allowed_region:?}",
            identity.hwnd
        );
        let expected = expected_owned_region(state, baseline.outer, allowed_region)?;
        let expected_bounds = (state == KnownRegionState::Absent)
            .then_some(allowed_region)
            .flatten();
        install_progress_owned_region(identity, &expected, expected_bounds, sample)?;
    }
    Ok(())
}

unsafe fn verify_known_region(
    hwnd: HWND,
    expected: &SerializedRegion,
    state: &str,
) -> Result<(), String> {
    match expected {
        SerializedRegion::Absent => {
            eprintln!(
                "clipping-matrix-recovery hwnd={hwnd:?} state={state}-known-absent-established-by-explicit-clear-no-getwindowrgn-absence-inference"
            );
            Ok(())
        }
        SerializedRegion::Data(_) => {
            if serialize_installed_region(hwnd)? != *expected {
                return Err(format!(
                    "MECHANICAL FAIL: region data did not match expected {state}"
                ));
            }
            Ok(())
        }
    }
}

unsafe fn verify_recovery_record(
    record: &RecoveryRecord,
    expected_current: &SerializedRegion,
    state: &str,
) -> Result<(), String> {
    verify_fixture_identity(record.identity)?;
    let dimensions = outer_client_bounds(
        record.identity.hwnd,
        record.identity.pid,
        record.identity.creation_filetime,
        state,
    )?;
    if dimensions != record.dimensions {
        return Err(format!(
            "MECHANICAL FAIL: {state} changed native outer or client dimensions"
        ));
    }
    if !ownership_matches(record.identity) {
        return Err(format!(
            "MECHANICAL FAIL: {state} lost the ownership marker"
        ));
    }
    verify_known_region(record.identity.hwnd, expected_current, state)
}

unsafe fn recover_owned_region(
    record: &RecoveryRecord,
    expected_current: &SerializedRegion,
    injection: &mut RestoreInjection,
) -> Result<bool, String> {
    verify_fixture_identity(record.identity)?;
    if !ownership_matches(record.identity) {
        eprintln!(
            "clipping-matrix-recovery hwnd={:?} state=preserved-unowned-or-replaced",
            record.identity.hwnd
        );
        return Ok(false);
    }
    if let Err(error) = verify_known_region(record.identity.hwnd, expected_current, "owned-current")
    {
        eprintln!(
            "clipping-matrix-recovery hwnd={:?} state=preserved-marker-region-mismatch error={error}",
            record.identity.hwnd
        );
        return Ok(false);
    }
    eprintln!(
        "clipping-matrix-recovery hwnd={:?} state=verified-before-restore race=query-to-mutate-unserialized",
        record.identity.hwnd
    );
    if let Err(first_error) =
        restore_serialized_region_injected(record.identity.hwnd, &record.original, injection)
    {
        if !first_error.starts_with("INJECTED:") {
            return Err(first_error);
        }
        verify_recovery_record(record, expected_current, "after-injected-restore-failure")?;
        eprintln!(
            "clipping-matrix-recovery hwnd={:?} state=restore-retry first-error={first_error}",
            record.identity.hwnd
        );
        restore_serialized_region_injected(record.identity.hwnd, &record.original, injection)
            .map_err(|retry_error| {
                format!(
                    "MECHANICAL FAIL: restoration retry failed after {first_error}; {retry_error}"
                )
            })?;
    }
    clear_ownership_property(record.identity.hwnd);
    let restored_dimensions = outer_client_bounds(
        record.identity.hwnd,
        record.identity.pid,
        record.identity.creation_filetime,
        "matrix-after-restore",
    )?;
    if restored_dimensions != record.dimensions {
        return Err(
            "MECHANICAL FAIL: restoration changed native outer or client dimensions".to_owned(),
        );
    }
    verify_known_region(record.identity.hwnd, &record.original, "restored")?;
    eprintln!(
        "clipping-matrix-recovery hwnd={:?} state=restored-owned-region",
        record.identity.hwnd
    );
    Ok(true)
}

unsafe fn repair_owned_fixture(record: &RecoveryRecord) -> Result<(), String> {
    restore_serialized_region(record.identity.hwnd, &record.original)?;
    clear_ownership_property(record.identity.hwnd);
    let dimensions = outer_client_bounds(
        record.identity.hwnd,
        record.identity.pid,
        record.identity.creation_filetime,
        "matrix-deliberate-cleanup-repair",
    )?;
    if dimensions != record.dimensions {
        return Err("MECHANICAL FAIL: deliberate repair changed native dimensions".to_owned());
    }
    verify_known_region(
        record.identity.hwnd,
        &record.original,
        "deliberate-cleanup-repair",
    )
}

fn parse_fixture_boot(line: &str) -> Result<(u32, u64), String> {
    let mut pid = None;
    let mut creation_filetime = None;
    for item in line.split_whitespace().skip(1) {
        let Some((key, value)) = item.split_once('=') else {
            continue;
        };
        match key {
            "pid" => pid = value.parse::<u32>().ok(),
            "creation_filetime" => creation_filetime = value.parse::<u64>().ok(),
            _ => {}
        }
    }
    match (pid, creation_filetime) {
        (Some(pid), Some(creation_filetime)) if creation_filetime != 0 => {
            Ok((pid, creation_filetime))
        }
        _ => Err(format!(
            "INCONCLUSIVE: malformed fixture boot line {line:?}"
        )),
    }
}

fn bind_retained_fixture(
    fixture: &Child,
    stdin: &mut ChildStdin,
    lines: &mpsc::Receiver<Result<String, String>>,
) -> Result<(), String> {
    let retained_pid = fixture.id();
    let retained_creation = unsafe { process_creation_filetime(retained_pid)? };
    let (reported_pid, reported_creation) =
        parse_fixture_boot(&wait_for_matrix_line(lines, "FIXTURE_BOOT")?)?;
    if (reported_pid, reported_creation) != (retained_pid, retained_creation) {
        return Err(format!(
            "INCONCLUSIVE: fixture boot identity did not match retained child pid={retained_pid} creation_filetime={retained_creation}"
        ));
    }
    eprintln!(
        "clipping-matrix-supervisor state=fixture-retained-identity pid={retained_pid} creation_filetime={retained_creation}"
    );
    send_matrix_command(stdin, &format!("bind {retained_pid} {retained_creation}"))
}

fn parse_matrix_identity(line: &str) -> Result<FixtureIdentity, String> {
    let mut hwnd = None;
    let mut pid = None;
    let mut creation_filetime = None;
    let mut generation = None;
    for item in line.split_whitespace().skip(1) {
        let Some((key, value)) = item.split_once('=') else {
            continue;
        };
        match key {
            "hwnd" => hwnd = value.parse::<isize>().ok(),
            "pid" => pid = value.parse::<u32>().ok(),
            "creation_filetime" => creation_filetime = value.parse::<u64>().ok(),
            "generation" => generation = value.parse::<usize>().ok(),
            _ => {}
        }
    }
    match (hwnd, pid, creation_filetime, generation) {
        (Some(hwnd), Some(pid), Some(creation_filetime), Some(generation)) if generation != 0 => {
            Ok(FixtureIdentity {
                hwnd: HWND(hwnd as *mut c_void),
                pid,
                creation_filetime,
                generation,
            })
        }
        _ => Err(format!(
            "INCONCLUSIVE: malformed fixture identity line {line:?}"
        )),
    }
}

fn start_matrix_output_reader(output: ChildStdout) -> mpsc::Receiver<Result<String, String>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            let result = line
                .map_err(|error| format!("INCONCLUSIVE: fixture protocol read failed: {error}"));
            let done = result.is_err();
            if sender.send(result).is_err() || done {
                break;
            }
        }
    });
    receiver
}

fn wait_for_matrix_line(
    receiver: &mpsc::Receiver<Result<String, String>>,
    prefix: &str,
) -> Result<String, String> {
    let deadline = Instant::now() + MATRIX_PROTOCOL_WAIT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!(
                "INCONCLUSIVE: matrix deadline waiting for {prefix}"
            ));
        }
        match receiver.recv_timeout(remaining) {
            Ok(Ok(line)) if line.starts_with(prefix) => return Ok(line),
            Ok(Ok(line)) => eprintln!("clipping-matrix-protocol {line}"),
            Ok(Err(error)) => return Err(error),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(format!(
                    "INCONCLUSIVE: matrix deadline waiting for {prefix}"
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(format!("INCONCLUSIVE: protocol closed before {prefix}"));
            }
        }
    }
}

fn send_matrix_command(stdin: &mut ChildStdin, command: &str) -> Result<(), String> {
    writeln!(stdin, "{command}")
        .and_then(|_| stdin.flush())
        .map_err(|error| format!("INCONCLUSIVE: failed to send fixture command {command}: {error}"))
}

struct RetainedMatrixFixture {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
}

impl RetainedMatrixFixture {
    fn new(mut child: Child) -> Self {
        let stdin = child
            .stdin
            .take()
            .expect("fixture stdin is guaranteed by Stdio::piped");
        let stdout = child
            .stdout
            .take()
            .expect("fixture stdout is guaranteed by Stdio::piped");
        Self {
            child,
            stdin: Some(stdin),
            stdout: Some(stdout),
        }
    }

    fn take_stdin(&mut self) -> ChildStdin {
        self.stdin
            .take()
            .expect("fixture stdin is retained exactly once")
    }

    fn take_stdout(&mut self) -> ChildStdout {
        self.stdout
            .take()
            .expect("fixture stdout is retained exactly once")
    }

    fn shutdown(&mut self, stdin: Option<&mut ChildStdin>) -> Result<(), String> {
        let command = match stdin {
            Some(stdin) => send_matrix_command(stdin, "exit"),
            None => match self.stdin.as_mut() {
                Some(stdin) => send_matrix_command(stdin, "exit"),
                None => Err("INCONCLUSIVE: retained fixture has no command stream".to_owned()),
            },
        };
        let waited = wait_for_matrix_child(&mut self.child, "fixture");
        match (command, waited) {
            (Ok(()), Ok(status)) if status.success() => {
                eprintln!("clipping-matrix-supervisor state=fixture-reaped exit={status}");
                Ok(())
            }
            (Ok(()), Ok(status)) => {
                Err(format!("INCONCLUSIVE: retained fixture exited as {status}"))
            }
            (Err(command), Ok(status)) => Err(format!(
                "{command}; retained fixture exit after failed graceful command: {status}"
            )),
            (Ok(()), Err(wait)) => Err(wait),
            (Err(command), Err(wait)) => Err(format!("{command}; fixture cleanup: {wait}")),
        }
    }
}

impl std::ops::Deref for RetainedMatrixFixture {
    type Target = Child;

    fn deref(&self) -> &Self::Target {
        &self.child
    }
}

impl std::ops::DerefMut for RetainedMatrixFixture {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.child
    }
}

impl Drop for RetainedMatrixFixture {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        if let Err(error) = self.shutdown(None) {
            eprintln!("clipping-matrix-supervisor state=fixture-drop-cleanup-failed error={error}");
        }
    }
}

struct RetainedMatrixController(Child);

impl RetainedMatrixController {
    fn new(child: Child) -> Self {
        Self(child)
    }

    fn cleanup(&mut self) -> Result<(), String> {
        match self.0.try_wait() {
            Ok(Some(status)) => {
                eprintln!("clipping-matrix-supervisor state=controller-reaped exit={status}");
                Ok(())
            }
            Ok(None) => {
                self.0.kill().map_err(|error| {
                    format!("INCONCLUSIVE: failed to terminate retained controller during cleanup: {error}")
                })?;
                let status = self.0.wait().map_err(|error| {
                    format!(
                        "INCONCLUSIVE: failed to reap retained controller during cleanup: {error}"
                    )
                })?;
                eprintln!("clipping-matrix-supervisor state=controller-terminated-for-cleanup exit={status}");
                Ok(())
            }
            Err(error) => Err(format!(
                "INCONCLUSIVE: failed to query retained controller during cleanup: {error}"
            )),
        }
    }
}

impl std::ops::Deref for RetainedMatrixController {
    type Target = Child;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for RetainedMatrixController {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for RetainedMatrixController {
    fn drop(&mut self) {
        match self.0.try_wait() {
            Ok(Some(status)) => eprintln!(
                "clipping-matrix-supervisor state=controller-reaped-on-drop exit={status}"
            ),
            Ok(None) => {
                let killed = self.0.kill();
                let waited = self.0.wait();
                eprintln!(
                    "clipping-matrix-supervisor state=controller-forced-cleanup killed={} exit={:?}",
                    killed.is_ok(),
                    waited.ok()
                );
            }
            Err(error) => eprintln!(
                "clipping-matrix-supervisor state=controller-cleanup-query-failed error={error}"
            ),
        }
    }
}

fn boundary_exit_code_is_unexpected(code: Option<i32>) -> bool {
    code != Some(1)
}

fn terminate_retained_controller(
    child: &mut Child,
    boundary: MatrixBoundary,
) -> Result<(), String> {
    if let Some(status) = child.try_wait().map_err(|error| {
        format!("INCONCLUSIVE: failed to query retained controller before termination: {error}")
    })? {
        return Err(format!(
            "INCONCLUSIVE: controller exited before intended termination at {} with {status}",
            boundary.phase()
        ));
    }
    if let Err(kill_error) = child.kill() {
        return match child.wait() {
            Ok(status) => Err(format!(
                "INCONCLUSIVE: failed to terminate retained controller at {}: {kill_error}; reaped retained controller as {status}",
                boundary.phase()
            )),
            Err(wait_error) => Err(format!(
                "INCONCLUSIVE: failed to terminate retained controller at {}: {kill_error}; failed to reap retained controller: {wait_error}",
                boundary.phase()
            )),
        };
    }
    let status = child
        .wait()
        .map_err(|error| format!("INCONCLUSIVE: failed to reap retained controller: {error}"))?;
    if boundary_exit_code_is_unexpected(status.code()) {
        return Err(format!(
            "INCONCLUSIVE: controller termination at {} lacked intentional-kill evidence; reaped status {status}",
            boundary.phase()
        ));
    }
    eprintln!(
        "clipping-matrix-supervisor state=controller-terminated boundary={} exit={status}",
        boundary.phase()
    );
    Ok(())
}

fn wait_for_matrix_child(child: &mut Child, role: &str) -> Result<ExitStatus, String> {
    let deadline = Instant::now() + MATRIX_PROTOCOL_WAIT;
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("INCONCLUSIVE: failed to wait for {role}: {error}"))?
        {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "INCONCLUSIVE: retained {role} exceeded {MATRIX_PROTOCOL_WAIT:?} protocol wait"
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

unsafe fn run_matrix_fixture(state: KnownRegionState, layout: MatrixLayout) -> Result<(), String> {
    let process_id = std::process::id();
    let creation_filetime = current_process_creation_filetime()?;
    let (command_tx, command_rx) = mpsc::channel();
    let _command_reader =
        thread::spawn(move || {
            for line in BufReader::new(std::io::stdin()).lines() {
                if command_tx
                    .send(line.map_err(|error| {
                        format!("INCONCLUSIVE: fixture command read failed: {error}")
                    }))
                    .is_err()
                {
                    break;
                }
            }
        });
    println!("FIXTURE_BOOT pid={process_id} creation_filetime={creation_filetime}");
    std::io::stdout()
        .flush()
        .map_err(|error| format!("INCONCLUSIVE: fixture boot flush failed: {error}"))?;
    let expected_bind = format!("bind {process_id} {creation_filetime}");
    match command_rx.recv_timeout(MATRIX_PROTOCOL_WAIT) {
        Ok(Ok(command)) if command == expected_bind => {}
        Ok(Ok(command)) => {
            return Err(format!(
                "INCONCLUSIVE: fixture rejected unbound command {command}"
            ))
        }
        Ok(Err(error)) => return Err(error),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            return Err(
                "INCONCLUSIVE: fixture was not bound to its retained child identity".to_owned(),
            )
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Err("INCONCLUSIVE: fixture binding channel closed".to_owned())
        }
    }

    let class_name: Vec<u16> = format!("LeopardWMClipMatrix-{process_id}-{creation_filetime}\0")
        .encode_utf16()
        .collect();
    let title: Vec<u16> = format!("LeopardWM clipping matrix {process_id} {creation_filetime}\0")
        .encode_utf16()
        .collect();
    let class = RegisteredClass::register(class_name)?;
    let mut fixture = OwnedFixtureWindow::create_with_bounds(
        PCWSTR(class.name.as_ptr()),
        PCWSTR(title.as_ptr()),
        4320,
        10,
        1600,
        1440,
        layout.ex_style(),
    )?;
    let mut generation = 1usize;
    install_known_region(fixture.hwnd, state)?;
    set_generation_property(fixture.hwnd, generation)?;
    assert_daemon_excluded_fixture(fixture.hwnd)?;
    if IsWindowVisible(fixture.hwnd).as_bool() {
        return Err(
            "INCONCLUSIVE: matrix fixture was visible before protocol admission".to_owned(),
        );
    }
    println!(
        "FIXTURE_READY hwnd={} pid={} creation_filetime={} generation={} state={}",
        fixture.hwnd.0 as isize,
        process_id,
        creation_filetime,
        generation,
        state.name()
    );
    std::io::stdout()
        .flush()
        .map_err(|error| format!("INCONCLUSIVE: fixture identity flush failed: {error}"))?;

    let result = loop {
        let mut message = MSG::default();
        while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        match command_rx.try_recv() {
            Ok(Ok(command)) if command == "exit" => break Ok(()),
            Ok(Ok(command)) if command == "recreate" => {
                if let Err(error) = fixture.cleanup() {
                    break Err(error);
                }
                generation += 1;
                match OwnedFixtureWindow::create_with_bounds(
                    PCWSTR(class.name.as_ptr()),
                    PCWSTR(title.as_ptr()),
                    4320,
                    10,
                    1600,
                    1440,
                    layout.ex_style(),
                ) {
                    Ok(created) => fixture = created,
                    Err(error) => break Err(error),
                }
                if let Err(error) = install_known_region(fixture.hwnd, state)
                    .and_then(|_| set_generation_property(fixture.hwnd, generation))
                {
                    break Err(error);
                }
                println!(
                    "FIXTURE_RECREATED hwnd={} pid={} creation_filetime={} generation={}",
                    fixture.hwnd.0 as isize, process_id, creation_filetime, generation
                );
                if let Err(error) = std::io::stdout().flush() {
                    break Err(format!(
                        "INCONCLUSIVE: recreated fixture identity flush failed: {error}"
                    ));
                }
            }
            Ok(Ok(command)) => {
                break Err(format!("INCONCLUSIVE: unknown fixture command {command}"))
            }
            Ok(Err(error)) => break Err(error),
            Err(mpsc::TryRecvError::Empty) => thread::sleep(Duration::from_millis(2)),
            Err(mpsc::TryRecvError::Disconnected) => {
                break Err("INCONCLUSIVE: fixture command channel closed".to_owned())
            }
        }
    };
    let cleanup = fixture.cleanup().map(|_| ());
    combine_primary_and_cleanup(result, cleanup, "fixture")
}

fn controller_phase(boundary: MatrixBoundary, phase: MatrixBoundary) -> Result<(), String> {
    println!("CONTROLLER_PHASE {}", phase.phase());
    std::io::stdout()
        .flush()
        .map_err(|error| format!("INCONCLUSIVE: controller phase flush failed: {error}"))?;
    if boundary == phase && phase != MatrixBoundary::Complete {
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    }
    Ok(())
}

unsafe fn run_matrix_controller(
    identity: FixtureIdentity,
    state: KnownRegionState,
    layout: MatrixLayout,
    boundary: MatrixBoundary,
    fault: Option<MatrixFault>,
) -> Result<(), String> {
    verify_fixture_identity(identity)?;
    if fault == Some(MatrixFault::Query) {
        return Err("INJECTED: query operation failed before GetWindowRect".to_owned());
    }
    let baseline = assert_matrix_fixture_geometry(identity, layout)?;
    progress_hidden_matrix_geometry(identity, baseline, fault)?;
    controller_phase(boundary, MatrixBoundary::BeforeMark)?;
    set_ownership_property(identity)?;
    controller_phase(boundary, MatrixBoundary::Marked)?;
    if fault == Some(MatrixFault::Install) {
        return Err("INJECTED: region install failed before SetWindowRgn".to_owned());
    }
    let expected = expected_owned_clip(state, baseline.outer, layout)?;
    install_owned_clip(identity, state, layout, &expected)?;
    progress_owned_clip_geometry(identity, state, layout, baseline)?;
    controller_phase(boundary, MatrixBoundary::Clipped)?;
    controller_phase(boundary, MatrixBoundary::BeforeRestore)?;
    if boundary == MatrixBoundary::ApplicationReplacementBeforeMarkerRemoval {
        let replacement = application_replacement_region()?;
        install_serialized_region(identity.hwnd, &replacement)?;
        controller_phase(
            boundary,
            MatrixBoundary::ApplicationReplacementBeforeMarkerRemoval,
        )?;
    }
    let original = serialized_known_region_shape(state)?;
    restore_serialized_region(identity.hwnd, &original)?;
    controller_phase(boundary, MatrixBoundary::RestoredBeforeMarkerRemoval)?;
    clear_ownership_property(identity.hwnd);
    controller_phase(boundary, MatrixBoundary::MarkerRemoved)?;
    if boundary == MatrixBoundary::ApplicationReplacementAfterMarkerRemoval {
        let replacement = application_replacement_region()?;
        install_serialized_region(identity.hwnd, &replacement)?;
        controller_phase(
            boundary,
            MatrixBoundary::ApplicationReplacementAfterMarkerRemoval,
        )?;
    }
    controller_phase(boundary, MatrixBoundary::Complete)
}

fn matrix_fixture_descriptor(identity: FixtureIdentity) -> String {
    format!(
        "{}:{}:{}:{}",
        identity.hwnd.0 as isize, identity.pid, identity.creation_filetime, identity.generation
    )
}

fn parse_matrix_fixture_descriptor(value: &str) -> Result<FixtureIdentity, String> {
    let mut values = value.split(':');
    let hwnd = values
        .next()
        .and_then(|value| value.parse::<isize>().ok())
        .ok_or_else(|| "INCONCLUSIVE: malformed controller HWND descriptor".to_owned())?;
    let pid = values
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| "INCONCLUSIVE: malformed controller PID descriptor".to_owned())?;
    let creation_filetime = values
        .next()
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| "INCONCLUSIVE: malformed controller creation descriptor".to_owned())?;
    let generation = values
        .next()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|generation| *generation != 0)
        .ok_or_else(|| "INCONCLUSIVE: malformed controller generation descriptor".to_owned())?;
    if values.next().is_some() {
        return Err("INCONCLUSIVE: malformed controller fixture descriptor".to_owned());
    }
    Ok(FixtureIdentity {
        hwnd: HWND(hwnd as *mut c_void),
        pid,
        creation_filetime,
        generation,
    })
}

fn run_matrix_with_deadline(
    role: &str,
    deadline: Duration,
    work: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let (stop_tx, stop_rx) = mpsc::channel();
    let role = role.to_owned();
    let watchdog = thread::Builder::new()
        .name(format!("clipping-matrix-{role}-deadline"))
        .spawn(move || {
            if stop_rx.recv_timeout(deadline).is_err() {
                eprintln!("clipping-matrix-{role} state=deadline-process-exit");
                std::process::exit(124);
            }
        })
        .map_err(|error| {
            format!("INCONCLUSIVE: failed to start matrix deadline watchdog: {error}")
        })?;
    let result = work();
    let _ = stop_tx.send(());
    let _ = watchdog.join();
    result
}

fn run_matrix_role() -> Result<(), String> {
    let role = std::env::var(MATRIX_ROLE_ENV)
        .map_err(|_| "INCONCLUSIVE: missing matrix process role".to_owned())?;
    let state = KnownRegionState::parse(
        &std::env::var(MATRIX_STATE_ENV)
            .map_err(|_| "INCONCLUSIVE: missing matrix region state".to_owned())?,
    )?;
    let layout = MatrixLayout::parse(std::env::var(MATRIX_LAYOUT_ENV).ok())?;
    match role.as_str() {
        "fixture" => run_matrix_with_deadline("fixture", MATRIX_FIXTURE_DEADLINE, || unsafe {
            run_matrix_fixture(state, layout)
        }),
        "controller" => {
            let identity = parse_matrix_fixture_descriptor(
                &std::env::var(MATRIX_FIXTURE_ENV)
                    .map_err(|_| "INCONCLUSIVE: missing controller fixture identity".to_owned())?,
            )?;
            let boundary = MatrixBoundary::parse(std::env::var(MATRIX_BOUNDARY_ENV).ok())?;
            let fault = MatrixFault::parse(std::env::var(MATRIX_FAULT_ENV).ok())?;
            let result =
                run_matrix_with_deadline("controller", MATRIX_CONTROLLER_DEADLINE, || unsafe {
                    run_matrix_controller(identity, state, layout, boundary, fault)
                });
            if let Some(fault) = fault {
                if fault.is_controller_fault()
                    && matches!(&result, Err(error) if error.starts_with("INJECTED:"))
                {
                    println!(
                        "CONTROLLER_FAULT_ACK fault={} intended-exit={MATRIX_EXPECTED_FAULT_EXIT}",
                        fault.name()
                    );
                    let _ = std::io::stdout().flush();
                    std::process::exit(MATRIX_EXPECTED_FAULT_EXIT);
                }
            }
            result
        }
        _ => Err(format!("INCONCLUSIVE: unknown matrix role {role}")),
    }
}

fn start_matrix_process(
    executable: &std::path::Path,
    role: &str,
    state: KnownRegionState,
    layout: MatrixLayout,
) -> Result<Child, String> {
    Command::new(executable)
        .args(["--exact", MATRIX_TEST_NAME, "--ignored", "--nocapture"])
        .env(OPT_IN_ENV, "1")
        .env(MATRIX_ROLE_ENV, role)
        .env(MATRIX_STATE_ENV, state.name())
        .env(MATRIX_LAYOUT_ENV, layout.name())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("INCONCLUSIVE: failed to launch retained {role}: {error}"))
}

fn start_matrix_controller(
    executable: &std::path::Path,
    state: KnownRegionState,
    layout: MatrixLayout,
    identity: FixtureIdentity,
    boundary: MatrixBoundary,
    fault: Option<MatrixFault>,
) -> Result<Child, String> {
    let mut command = Command::new(executable);
    command
        .args(["--exact", MATRIX_TEST_NAME, "--ignored", "--nocapture"])
        .env(OPT_IN_ENV, "1")
        .env(MATRIX_ROLE_ENV, "controller")
        .env(MATRIX_STATE_ENV, state.name())
        .env(MATRIX_LAYOUT_ENV, layout.name())
        .env(MATRIX_FIXTURE_ENV, matrix_fixture_descriptor(identity))
        .env(MATRIX_BOUNDARY_ENV, boundary.phase())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    if let Some(fault) = fault.filter(|fault| fault.is_controller_fault()) {
        command.env(MATRIX_FAULT_ENV, fault.name());
    }
    command
        .spawn()
        .map_err(|error| format!("INCONCLUSIVE: failed to launch retained controller: {error}"))
}

fn terminate_matrix_fixture(
    fixture: &mut RetainedMatrixFixture,
    stdin: &mut ChildStdin,
) -> Result<(), String> {
    fixture.shutdown(Some(stdin))
}

fn combine_primary_and_cleanup(
    primary: Result<(), String>,
    cleanup: Result<(), String>,
    label: &str,
) -> Result<(), String> {
    match (primary, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(primary), Ok(())) => Err(primary),
        (Ok(()), Err(cleanup)) => Err(format!("{label} cleanup: {cleanup}")),
        (Err(primary), Err(cleanup)) => Err(format!("{primary}; {label} cleanup: {cleanup}")),
    }
}

fn run_matrix_case(
    state: KnownRegionState,
    layout: MatrixLayout,
    boundary: MatrixBoundary,
    fault: Option<MatrixFault>,
) -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|error| {
        format!("INCONCLUSIVE: could not resolve matrix test executable: {error}")
    })?;
    let mut fixture =
        RetainedMatrixFixture::new(start_matrix_process(&executable, "fixture", state, layout)?);
    let mut fixture_stdin = fixture.take_stdin();
    let fixture_lines = start_matrix_output_reader(fixture.take_stdout());
    let identity = match (|| {
        bind_retained_fixture(&fixture, &mut fixture_stdin, &fixture_lines)?;
        parse_matrix_identity(&wait_for_matrix_line(&fixture_lines, "FIXTURE_READY")?)
    })() {
        Ok(identity) => identity,
        Err(error) => {
            return combine_primary_and_cleanup(
                Err(error),
                terminate_matrix_fixture(&mut fixture, &mut fixture_stdin),
                "fixture",
            );
        }
    };

    let outcome = (|| unsafe {
        verify_fixture_identity(identity)?;
        let dimensions = assert_matrix_fixture_geometry(identity, layout)?;
        let record = RecoveryRecord {
            identity,
            original: capture_known_original(identity.hwnd, state)?,
            expected_installed: expected_owned_clip(state, dimensions.outer, layout)?,
            dimensions,
        };
        let mut controller = RetainedMatrixController::new(start_matrix_controller(
            &executable,
            state,
            layout,
            identity,
            boundary,
            fault,
        )?);
        let controller_lines = start_matrix_output_reader(
            controller
                .stdout
                .take()
                .expect("controller stdout is guaranteed by Stdio::piped"),
        );
        let primary = (|| {
            if let Some(fault) = fault.filter(|fault| fault.is_controller_fault()) {
                let acknowledgment =
                    wait_for_matrix_line(&controller_lines, "CONTROLLER_FAULT_ACK")?;
                let expected_ack = format!(
                    "CONTROLLER_FAULT_ACK fault={} intended-exit={MATRIX_EXPECTED_FAULT_EXIT}",
                    fault.name()
                );
                if acknowledgment != expected_ack {
                    return Err(format!(
                        "INCONCLUSIVE: injected fault acknowledgment was {acknowledgment:?}, expected {expected_ack:?}"
                    ));
                }
                let status = wait_for_matrix_child(&mut controller, "injected-fault-controller")?;
                if status.code() != Some(MATRIX_EXPECTED_FAULT_EXIT) {
                    return Err(format!(
                        "INCONCLUSIVE: injected {} controller exit was {status}, not intended exit {MATRIX_EXPECTED_FAULT_EXIT}",
                        fault.name()
                    ));
                }
                verify_fixture_identity(identity)?;
                let current_dimensions = outer_client_bounds(
                    identity.hwnd,
                    identity.pid,
                    identity.creation_filetime,
                    "matrix-after-injected-controller-fault",
                )?;
                if current_dimensions != record.dimensions {
                    return Err(format!(
                        "MECHANICAL FAIL: injected {} fault changed native dimensions",
                        fault.name()
                    ));
                }
                match fault {
                    MatrixFault::Install => {
                        verify_recovery_record(&record, &record.original, "after-install-fault")?;
                        if !recover_owned_region(
                            &record,
                            &record.original,
                            &mut RestoreInjection::None,
                        )? {
                            return Err("MECHANICAL FAIL: install fault did not retain recoverable marker state".to_owned());
                        }
                    }
                    MatrixFault::Query | MatrixFault::Placement => {
                        if ownership_matches(identity) {
                            return Err(format!(
                                "MECHANICAL FAIL: injected {} fault created an ownership marker",
                                fault.name()
                            ));
                        }
                        verify_known_region(
                            identity.hwnd,
                            &record.original,
                            "after-controller-fault",
                        )?;
                    }
                    MatrixFault::RestoreOnce | MatrixFault::RestoreTerminal => unreachable!(),
                }
                return Ok(());
            }
            wait_for_matrix_line(
                &controller_lines,
                &format!("CONTROLLER_PHASE {}", boundary.phase()),
            )?;
            if boundary == MatrixBoundary::Complete {
                let status = wait_for_matrix_child(&mut controller, "controller")?;
                if !status.success() {
                    return Err(format!(
                        "INCONCLUSIVE: retained controller exited as {status}"
                    ));
                }
            } else {
                terminate_retained_controller(&mut controller, boundary)?;
            }

            let replacement = application_replacement_region()?;
            match boundary {
                MatrixBoundary::BeforeMark => {
                    if ownership_matches(identity)
                        || recover_owned_region(
                            &record,
                            &record.original,
                            &mut RestoreInjection::None,
                        )?
                    {
                        return Err(
                            "MECHANICAL FAIL: recovery restored an unmarked region".to_owned()
                        );
                    }
                    verify_known_region(identity.hwnd, &record.original, "before-mark-unchanged")?;
                }
                MatrixBoundary::MarkerRemoved | MatrixBoundary::Complete => {
                    if ownership_matches(identity)
                        || recover_owned_region(
                            &record,
                            &record.original,
                            &mut RestoreInjection::None,
                        )?
                    {
                        return Err(
                            "MECHANICAL FAIL: recovery admitted a marker-removed region".to_owned()
                        );
                    }
                    verify_known_region(
                        identity.hwnd,
                        &record.original,
                        "marker-removed-or-complete",
                    )?;
                }
                MatrixBoundary::ApplicationReplacementAfterMarkerRemoval => {
                    if ownership_matches(identity)
                        || recover_owned_region(&record, &replacement, &mut RestoreInjection::None)?
                    {
                        return Err(
                            "MECHANICAL FAIL: recovery overwrote unmarked application replacement"
                                .to_owned(),
                        );
                    }
                    verify_known_region(
                        identity.hwnd,
                        &replacement,
                        "application-replacement-after-marker-removal",
                    )?;
                    repair_owned_fixture(&record)?;
                }
                MatrixBoundary::ApplicationReplacementBeforeMarkerRemoval => {
                    verify_recovery_record(
                        &record,
                        &replacement,
                        "application-replacement-with-marker",
                    )?;
                    if recover_owned_region(
                        &record,
                        &record.expected_installed,
                        &mut RestoreInjection::None,
                    )? {
                        return Err("MECHANICAL FAIL: recovery overwrote application replacement with marker".to_owned());
                    }
                    verify_recovery_record(
                        &record,
                        &replacement,
                        "application-replacement-preserved-with-marker",
                    )?;
                    repair_owned_fixture(&record)?;
                }
                MatrixBoundary::Marked => {
                    if !recover_owned_region(
                        &record,
                        &record.original,
                        &mut RestoreInjection::None,
                    )? {
                        return Err(
                            "MECHANICAL FAIL: recovery did not clear marked unmodified state"
                                .to_owned(),
                        );
                    }
                }
                MatrixBoundary::RestoredBeforeMarkerRemoval => {
                    if !recover_owned_region(
                        &record,
                        &record.original,
                        &mut RestoreInjection::None,
                    )? {
                        return Err("MECHANICAL FAIL: recovery did not idempotently finish restored marker state".to_owned());
                    }
                }
                MatrixBoundary::Clipped | MatrixBoundary::BeforeRestore => {
                    if boundary == MatrixBoundary::Clipped {
                        eprintln!(
                            "clipping-matrix-supervisor state=controller-cancelled-with-owned-clip"
                        );
                    }
                    let mut injection = RestoreInjection::from_fault(fault);
                    match recover_owned_region(&record, &record.expected_installed, &mut injection)
                    {
                        Ok(true) if fault == Some(MatrixFault::RestoreTerminal) => {
                            return Err(
                                "MECHANICAL FAIL: terminal restore failure unexpectedly succeeded"
                                    .to_owned(),
                            );
                        }
                        Ok(true) => {}
                        Ok(false) => {
                            return Err("MECHANICAL FAIL: recovery did not restore owned clipping"
                                .to_owned())
                        }
                        Err(error) if fault == Some(MatrixFault::RestoreTerminal) => {
                            if !error.contains("INJECTED: restore failed before SetWindowRgn") {
                                return Err(error);
                            }
                            verify_recovery_record(
                                &record,
                                &record.expected_installed,
                                "after-terminal-restore-failure",
                            )?;
                            repair_owned_fixture(&record)?;
                            return Ok(());
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
            if recover_owned_region(&record, &record.original, &mut RestoreInjection::None)? {
                return Err("MECHANICAL FAIL: completed recovery was not idempotent".to_owned());
            }
            verify_known_region(identity.hwnd, &record.original, "post-recovery-idempotence")
        })();
        combine_primary_and_cleanup(primary, controller.cleanup(), "controller")
    })();

    combine_primary_and_cleanup(
        outcome,
        terminate_matrix_fixture(&mut fixture, &mut fixture_stdin),
        "fixture",
    )
}

unsafe fn run_matrix_stale_identity_case(
    state: KnownRegionState,
    layout: MatrixLayout,
) -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|error| {
        format!("INCONCLUSIVE: could not resolve matrix test executable: {error}")
    })?;
    let mut fixture =
        RetainedMatrixFixture::new(start_matrix_process(&executable, "fixture", state, layout)?);
    let mut fixture_stdin = fixture.take_stdin();
    let fixture_lines = start_matrix_output_reader(fixture.take_stdout());
    let identity = match (|| {
        bind_retained_fixture(&fixture, &mut fixture_stdin, &fixture_lines)?;
        parse_matrix_identity(&wait_for_matrix_line(&fixture_lines, "FIXTURE_READY")?)
    })() {
        Ok(identity) => identity,
        Err(error) => {
            return combine_primary_and_cleanup(
                Err(error),
                terminate_matrix_fixture(&mut fixture, &mut fixture_stdin),
                "fixture",
            );
        }
    };
    let outcome = (|| {
        let dimensions = assert_matrix_fixture_geometry(identity, layout)?;
        let record = RecoveryRecord {
            identity,
            original: capture_known_original(identity.hwnd, state)?,
            expected_installed: expected_owned_clip(state, dimensions.outer, layout)?,
            dimensions,
        };
        let mut controller = RetainedMatrixController::new(start_matrix_controller(
            &executable,
            state,
            layout,
            identity,
            MatrixBoundary::Marked,
            None,
        )?);
        let lines = start_matrix_output_reader(
            controller
                .stdout
                .take()
                .expect("controller stdout is guaranteed by Stdio::piped"),
        );
        let primary = (|| {
            wait_for_matrix_line(&lines, "CONTROLLER_PHASE marked")?;
            terminate_retained_controller(&mut controller, MatrixBoundary::Marked)?;
            set_generation_property(identity.hwnd, identity.generation + 1)?;
            if !recover_owned_region(&record, &record.original, &mut RestoreInjection::None)
                .is_err()
            {
                return Err(
                    "MECHANICAL FAIL: stale generation was admitted for restoration".to_owned(),
                );
            }
            set_generation_property(identity.hwnd, identity.generation)?;
            if !recover_owned_region(&record, &record.original, &mut RestoreInjection::None)? {
                return Err(
                    "MECHANICAL FAIL: restored generation did not recover marked state".to_owned(),
                );
            }
            send_matrix_command(&mut fixture_stdin, "recreate")?;
            let recreated =
                parse_matrix_identity(&wait_for_matrix_line(&fixture_lines, "FIXTURE_RECREATED")?)?;
            if recreated == identity || !verify_fixture_identity(identity).is_err() {
                return Err(
                    "MECHANICAL FAIL: stale HWND generation was admitted after fixture recreation"
                        .to_owned(),
                );
            }
            Ok(())
        })();
        combine_primary_and_cleanup(primary, controller.cleanup(), "controller")
    })();
    combine_primary_and_cleanup(
        outcome,
        terminate_matrix_fixture(&mut fixture, &mut fixture_stdin),
        "fixture",
    )
}

#[test]
fn aligned_region_data_preserves_exact_bytes() {
    let bytes = vec![1, 2, 3, 4, 5];
    let data = AlignedRegionData::from_bytes(&bytes).unwrap();
    assert_eq!(data.byte_len, bytes.len());
    assert_eq!(
        (data.as_rgndata() as usize) % std::mem::align_of::<RGNDATA>(),
        0
    );
    assert_eq!(data.into_bytes(), bytes);
}

#[test]
fn matrix_deadlines_and_boundary_exit_codes_are_ordered() {
    assert!(MATRIX_PROTOCOL_WAIT < MATRIX_CONTROLLER_DEADLINE);
    assert!(MATRIX_CONTROLLER_DEADLINE < MATRIX_FIXTURE_DEADLINE);
    assert!(MATRIX_FIXTURE_DEADLINE < MATRIX_SUPERVISOR_DEADLINE);
    assert!(MATRIX_SUPERVISOR_DEADLINE < MATRIX_EXTERNAL_SUPERVISOR_DEADLINE);
    assert!(boundary_exit_code_is_unexpected(None));
    assert!(boundary_exit_code_is_unexpected(Some(0)));
    assert!(boundary_exit_code_is_unexpected(Some(101)));
    assert!(boundary_exit_code_is_unexpected(Some(
        0xC000_0005u32 as i32
    )));
    assert!(boundary_exit_code_is_unexpected(Some(124)));
    assert!(boundary_exit_code_is_unexpected(Some(
        MATRIX_EXPECTED_FAULT_EXIT
    )));
    assert!(!boundary_exit_code_is_unexpected(Some(1)));
}

#[test]
fn synthetic_outer_relative_frame_coordinates_are_origin_and_direction_independent() {
    let outer = Bounds {
        left: -2400,
        top: 120,
        right: -800,
        bottom: 1560,
    };
    let extended_frame = Bounds {
        left: -2392,
        top: 120,
        right: -808,
        bottom: 1552,
    };
    assert_eq!(
        extended_frame.relative_to(outer),
        Bounds {
            left: 8,
            top: 0,
            right: 1592,
            bottom: 1432,
        }
    );
    assert_eq!(
        MatrixLayout::Ltr.allowed_slice(),
        Bounds {
            left: 0,
            top: 0,
            right: 800,
            bottom: 1430,
        }
    );
    assert_eq!(
        MatrixLayout::Rtl.allowed_slice(),
        Bounds {
            left: 800,
            top: 0,
            right: 1600,
            bottom: 1430,
        }
    );
}

#[test]
fn synthetic_visible_slices_preserve_native_frame_dimensions() {
    let owner = Bounds {
        left: 0,
        top: 0,
        right: 5120,
        bottom: 1440,
    };
    let neighbor = Bounds {
        left: 5120,
        top: 0,
        right: 5920,
        bottom: 600,
    };
    let frame = Bounds {
        left: 4320,
        top: 10,
        right: 5920,
        bottom: 1450,
    };
    let right = synthetic_visible_slice(frame, owner).unwrap();
    assert_eq!(frame.width(), 1600);
    assert_eq!(right.width(), 800);
    assert_eq!(
        right,
        Bounds {
            left: 0,
            top: 0,
            right: 800,
            bottom: 1430
        }
    );
    assert_eq!(
        synthetic_visible_slice(frame, neighbor).unwrap(),
        Bounds {
            left: 800,
            top: 0,
            right: 1600,
            bottom: 590
        }
    );
    assert!(synthetic_visible_slice(
        frame,
        Bounds {
            left: 7000,
            top: 0,
            right: 8000,
            bottom: 100
        }
    )
    .is_none());
}

#[test]
fn synthetic_progression_covers_left_top_bottom_recenter_and_parking() {
    let frame = Bounds {
        left: 100,
        top: 100,
        right: 500,
        bottom: 400,
    };
    let left = Bounds {
        left: 200,
        top: 100,
        right: 600,
        bottom: 400,
    };
    let top = Bounds {
        left: 100,
        top: 200,
        right: 500,
        bottom: 500,
    };
    let bottom = Bounds {
        left: 100,
        top: 0,
        right: 500,
        bottom: 250,
    };
    assert_eq!(synthetic_visible_slice(frame, left).unwrap().left, 100);
    assert_eq!(synthetic_visible_slice(frame, top).unwrap().top, 100);
    assert_eq!(synthetic_visible_slice(frame, bottom).unwrap().bottom, 150);
    assert_eq!(
        synthetic_visible_slice(frame, frame).unwrap(),
        Bounds {
            left: 0,
            top: 0,
            right: 400,
            bottom: 300
        }
    );
    assert!(synthetic_visible_slice(
        frame,
        Bounds {
            left: -1000,
            top: -1000,
            right: -1,
            bottom: -1
        }
    )
    .is_none());
}

#[test]
fn framed_window_evaluation_reports_presentation_separately() {
    let evidence = FramedWindowEvidence {
        hidden_baseline: OuterClientBounds {
            outer: Bounds {
                left: 24,
                top: 24,
                right: 384,
                bottom: 264,
            },
            client: Bounds {
                left: 0,
                top: 0,
                right: 344,
                bottom: 201,
            },
        },
        composed_baseline: ComposedMeasurement {
            outer: Bounds {
                left: 24,
                top: 24,
                right: 384,
                bottom: 264,
            },
            client: Bounds {
                left: 0,
                top: 0,
                right: 344,
                bottom: 201,
            },
            extended_frame_relative_to_outer: Bounds {
                left: 7,
                top: 0,
                right: 353,
                bottom: 232,
            },
            nc_rendering_enabled: true,
        },
        clipped: ComposedMeasurement {
            outer: Bounds {
                left: 24,
                top: 24,
                right: 384,
                bottom: 264,
            },
            client: Bounds {
                left: 0,
                top: 0,
                right: 344,
                bottom: 201,
            },
            extended_frame_relative_to_outer: Bounds {
                left: 7,
                top: 0,
                right: 180,
                bottom: 232,
            },
            nc_rendering_enabled: false,
        },
        cleared: ComposedMeasurement {
            outer: Bounds {
                left: 24,
                top: 24,
                right: 384,
                bottom: 264,
            },
            client: Bounds {
                left: 0,
                top: 0,
                right: 344,
                bottom: 201,
            },
            extended_frame_relative_to_outer: Bounds {
                left: 7,
                top: 0,
                right: 353,
                bottom: 232,
            },
            nc_rendering_enabled: true,
        },
        installed_clip_bounds: Bounds {
            left: 0,
            top: 0,
            right: 180,
            bottom: 240,
        },
        queried_clip_bounds: Bounds {
            left: 0,
            top: 0,
            right: 180,
            bottom: 240,
        },
    };

    assert_eq!(
        evaluate(&evidence),
        MechanicalVerdict::Pass {
            presentation_limitations: vec![
                "non-client rendering changed while clipped",
                "extended frame bounds changed while clipped",
            ],
        }
    );
}

#[test]
fn framed_window_evaluation_rejects_changed_native_size() {
    let baseline = Bounds {
        left: 24,
        top: 24,
        right: 384,
        bottom: 264,
    };
    let client = Bounds {
        left: 0,
        top: 0,
        right: 344,
        bottom: 201,
    };
    let frame = Bounds {
        left: 7,
        top: 0,
        right: 353,
        bottom: 232,
    };
    let mut evidence = FramedWindowEvidence {
        hidden_baseline: OuterClientBounds {
            outer: baseline,
            client,
        },
        composed_baseline: ComposedMeasurement {
            outer: baseline,
            client,
            extended_frame_relative_to_outer: frame,
            nc_rendering_enabled: true,
        },
        clipped: ComposedMeasurement {
            outer: Bounds {
                right: 204,
                ..baseline
            },
            client,
            extended_frame_relative_to_outer: frame,
            nc_rendering_enabled: true,
        },
        cleared: ComposedMeasurement {
            outer: baseline,
            client,
            extended_frame_relative_to_outer: frame,
            nc_rendering_enabled: true,
        },
        installed_clip_bounds: Bounds {
            left: 0,
            top: 0,
            right: 180,
            bottom: 240,
        },
        queried_clip_bounds: Bounds {
            left: 0,
            top: 0,
            right: 180,
            bottom: 240,
        },
    };

    assert!(matches!(evaluate(&evidence), MechanicalVerdict::Fail(_)));
    evidence.clipped.outer = baseline;
    evidence
        .composed_baseline
        .extended_frame_relative_to_outer
        .right = 0;
    assert!(matches!(evaluate(&evidence), MechanicalVerdict::Fail(_)));
}

#[test]
#[ignore = "Creates only hidden daemon-excluded fixture windows in separately retained fixture processes. Run the exact built test executable with LEOPARDWM_RUN_FRAMED_WINDOW_CLIPPING_PROOF=1; the supervisor kills only retained disposable controller children at named boundaries."]
fn known_state_region_ownership_recovery_matrix() {
    if std::env::var(OPT_IN_ENV).as_deref() != Ok("1") {
        panic!("INCONCLUSIVE: set {OPT_IN_ENV}=1 before running this exact ignored native proof");
    }
    let _dpi = unsafe { TestDpiContext::enter() }.unwrap_or_else(|reason| panic!("{reason}"));
    if std::env::var_os(MATRIX_ROLE_ENV).is_some() {
        run_matrix_role().unwrap_or_else(|reason| panic!("{reason}"));
        return;
    }

    let (stop_tx, stop_rx) = mpsc::channel();
    let watchdog = thread::spawn(move || {
        if stop_rx.recv_timeout(MATRIX_SUPERVISOR_DEADLINE).is_err() {
            eprintln!("clipping-matrix-supervisor state=whole-test-deadline-process-exit");
            std::process::exit(124);
        }
    });
    for (state, layout) in [
        (KnownRegionState::Absent, MatrixLayout::Ltr),
        (KnownRegionState::Empty, MatrixLayout::Ltr),
        (KnownRegionState::Simple, MatrixLayout::Ltr),
        (KnownRegionState::Complex, MatrixLayout::Ltr),
        (KnownRegionState::Absent, MatrixLayout::Rtl),
    ] {
        for boundary in [
            MatrixBoundary::BeforeMark,
            MatrixBoundary::Marked,
            MatrixBoundary::Clipped,
            MatrixBoundary::BeforeRestore,
            MatrixBoundary::RestoredBeforeMarkerRemoval,
            MatrixBoundary::MarkerRemoved,
            MatrixBoundary::ApplicationReplacementBeforeMarkerRemoval,
            MatrixBoundary::ApplicationReplacementAfterMarkerRemoval,
            MatrixBoundary::Complete,
        ] {
            run_matrix_case(state, layout, boundary, None).unwrap_or_else(|reason| {
                panic!(
                    "matrix state={} layout={} boundary={}: {reason}",
                    state.name(),
                    layout.name(),
                    boundary.phase()
                )
            });
        }
        for fault in [
            MatrixFault::Query,
            MatrixFault::Placement,
            MatrixFault::Install,
        ] {
            run_matrix_case(state, layout, MatrixBoundary::Complete, Some(fault)).unwrap_or_else(
                |reason| {
                    panic!(
                        "matrix state={} layout={} injected-fault={}: {reason}",
                        state.name(),
                        layout.name(),
                        fault.name()
                    )
                },
            );
        }
        for fault in [MatrixFault::RestoreOnce, MatrixFault::RestoreTerminal] {
            run_matrix_case(state, layout, MatrixBoundary::Clipped, Some(fault)).unwrap_or_else(
                |reason| {
                    panic!(
                        "matrix state={} layout={} injected-fault={}: {reason}",
                        state.name(),
                        layout.name(),
                        fault.name()
                    )
                },
            );
        }
        unsafe {
            run_matrix_stale_identity_case(state, layout).unwrap_or_else(|reason| {
                panic!(
                    "matrix state={} layout={} stale-identity: {reason}",
                    state.name(),
                    layout.name()
                )
            });
        }
    }
    let _ = stop_tx.send(());
    let _ = watchdog.join();
}

#[test]
#[ignore = "Creates one owned framed WS_OVERLAPPEDWINDOW fixture only when explicitly opted in. Run the exact test with LEOPARDWM_RUN_FRAMED_WINDOW_CLIPPING_PROOF=1; its retained child has a 12-second process deadline and its parent supervisor kills only that child at 13 seconds."]
fn framed_window_region_viability() {
    if std::env::var(OPT_IN_ENV).as_deref() != Ok("1") {
        panic!("INCONCLUSIVE: set {OPT_IN_ENV}=1 before running this exact ignored native proof");
    }

    if std::env::var_os(CHILD_ENV).is_some() {
        match run_child_fixture() {
            Ok(MechanicalVerdict::Pass {
                presentation_limitations,
            }) => {
                eprintln!(
                    "framed-clipping-fixture state=mechanical-pass presentation-limitations={} visual-evidence=unavailable(shadows,corners,compositor-containment,visual-equivalence)",
                    presentation_limitations.join("|")
                );
            }
            Ok(MechanicalVerdict::Fail(reason)) => panic!("MECHANICAL FAIL: {reason}"),
            Err(reason) => panic!("{reason}"),
        }
        return;
    }

    let executable = std::env::current_exe()
        .expect("INCONCLUSIVE: could not resolve the current test executable for supervision");
    let mut child = Command::new(executable)
        .args(["--exact", TEST_NAME, "--ignored", "--nocapture"])
        .env(OPT_IN_ENV, "1")
        .env(CHILD_ENV, "1")
        .spawn()
        .expect("INCONCLUSIVE: failed to launch retained fixture child");
    let status = wait_for_supervised_child(&mut child).unwrap_or_else(|reason| panic!("{reason}"));
    assert!(
        status.success(),
        "INCONCLUSIVE: retained fixture child exited as {status}"
    );
}
