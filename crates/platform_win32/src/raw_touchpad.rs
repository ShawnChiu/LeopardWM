//! Opt-in Precision Touchpad Raw Input acquisition and discrete swipe recognition.

use std::{
    collections::{HashMap, HashSet},
    mem::size_of,
    time::{Duration, Instant},
};
use windows::{
    core::w,
    Win32::{
        Devices::HumanInterfaceDevice::{
            HidP_GetButtonCaps, HidP_GetCaps, HidP_GetUsageValue, HidP_GetUsages,
            HidP_GetValueCaps, HidP_Input, HIDP_BUTTON_CAPS, HIDP_CAPS, HIDP_STATUS_SUCCESS,
            HIDP_VALUE_CAPS, PHIDP_PREPARSED_DATA,
        },
        Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM},
        UI::{
            Input::{
                GetRawInputData, GetRawInputDeviceInfoW, GetRawInputDeviceList,
                GetRegisteredRawInputDevices, RegisterRawInputDevices, HRAWINPUT, RAWINPUT,
                RAWINPUTDEVICE, RAWINPUTDEVICELIST, RAWINPUTHEADER, RIDEV_INPUTSINK,
                RIDEV_PAGEONLY, RIDEV_REMOVE, RIDI_PREPARSEDDATA, RID_INPUT, RIM_TYPEHID,
            },
            WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, UnregisterClassW,
                MSG, WINDOW_EX_STYLE, WINDOW_STYLE, WM_INPUT, WNDCLASSW,
            },
        },
    },
};

const DIGITIZER: u16 = 0x0D;
const TOUCHPAD: u16 = 0x05;
const CONTACT_COUNT: u16 = 0x54;
const SCAN_TIME: u16 = 0x56;
const CONTACT_ID: u16 = 0x51;
const TIP: u16 = 0x42;
const CONFIDENCE: u16 = 0x47;
const DESKTOP: u16 = 0x01;
const X: u16 = 0x30;
const Y: u16 = 0x31;
const MAX_INPUT_BYTES: usize = 64 * 1024;
const MAX_CONTACTS: usize = 5;
const GESTURE_GAP: Duration = Duration::from_millis(300);
const SWIPE_TRAVEL: f32 = 0.14;
const AXIS_DOMINANCE: f32 = 1.5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Swipe {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Contact {
    id: u32,
    x: f32,
    y: f32,
}

#[derive(Clone, Copy)]
struct ContactCaps {
    link: u16,
    x_min: i32,
    x_span: f32,
    y_min: i32,
    y_span: f32,
}

struct Device {
    // HidP consumes a borrowed preparsed-data pointer. u64 provides alignment.
    preparsed: Vec<u64>,
    contacts: Vec<ContactCaps>,
    frame: FrameAssembler,
    swipe: SwipeEngine,
}

impl Device {
    fn open(handle: HANDLE) -> Result<Self, String> {
        let mut bytes = 0u32;
        if unsafe { GetRawInputDeviceInfoW(Some(handle), RIDI_PREPARSEDDATA, None, &mut bytes) }
            == u32::MAX
            || bytes == 0
            || bytes as usize > MAX_INPUT_BYTES
        {
            return Err("preparsed data unavailable".into());
        }
        let mut preparsed = vec![0u64; (bytes as usize).div_ceil(8)];
        if unsafe {
            GetRawInputDeviceInfoW(
                Some(handle),
                RIDI_PREPARSEDDATA,
                Some(preparsed.as_mut_ptr().cast()),
                &mut bytes,
            )
        } == u32::MAX
        {
            return Err("preparsed data read failed".into());
        }
        let pointer = PHIDP_PREPARSED_DATA(preparsed.as_ptr() as isize);
        let mut caps = HIDP_CAPS::default();
        if unsafe { HidP_GetCaps(pointer, &mut caps) } != HIDP_STATUS_SUCCESS
            || (caps.UsagePage, caps.Usage) != (DIGITIZER, TOUCHPAD)
        {
            return Err("not a Precision Touchpad collection".into());
        }
        if caps.NumberInputValueCaps > 512 || caps.NumberInputButtonCaps > 512 {
            return Err("touchpad capability count is unreasonable".into());
        }
        let mut values = vec![HIDP_VALUE_CAPS::default(); caps.NumberInputValueCaps as usize];
        let mut value_count = caps.NumberInputValueCaps;
        if value_count == 0
            || unsafe {
                HidP_GetValueCaps(HidP_Input, values.as_mut_ptr(), &mut value_count, pointer)
            } != HIDP_STATUS_SUCCESS
        {
            return Err("input value capabilities unavailable".into());
        }
        values.truncate(value_count as usize);
        let mut buttons = vec![HIDP_BUTTON_CAPS::default(); caps.NumberInputButtonCaps as usize];
        let mut button_count = caps.NumberInputButtonCaps;
        if button_count == 0
            || unsafe {
                HidP_GetButtonCaps(HidP_Input, buttons.as_mut_ptr(), &mut button_count, pointer)
            } != HIDP_STATUS_SUCCESS
        {
            return Err("input button capabilities unavailable".into());
        }
        buttons.truncate(button_count as usize);
        if !values
            .iter()
            .any(|cap| has_value(cap, DIGITIZER, CONTACT_COUNT))
            || !values
                .iter()
                .any(|cap| has_value(cap, DIGITIZER, SCAN_TIME))
        {
            return Err("contact count or scan time is missing".into());
        }
        let mut links = HashSet::new();
        for cap in &values {
            if has_value(cap, DIGITIZER, CONTACT_ID) {
                links.insert(cap.LinkCollection);
            }
        }
        let mut contacts = Vec::new();
        for link in links {
            let axis = |usage| {
                values
                    .iter()
                    .find(|cap| cap.LinkCollection == link && has_value(cap, DESKTOP, usage))
            };
            let (Some(x), Some(y)) = (axis(X), axis(Y)) else {
                continue;
            };
            if x.LogicalMin < 0
                || y.LogicalMin < 0
                || x.LogicalMax <= x.LogicalMin
                || y.LogicalMax <= y.LogicalMin
                || !buttons
                    .iter()
                    .any(|cap| cap.LinkCollection == link && has_button(cap, TIP))
                || !buttons
                    .iter()
                    .any(|cap| cap.LinkCollection == link && has_button(cap, CONFIDENCE))
            {
                continue;
            }
            contacts.push(ContactCaps {
                link,
                x_min: x.LogicalMin,
                x_span: (x.LogicalMax as i64 - x.LogicalMin as i64) as f32,
                y_min: y.LogicalMin,
                y_span: (y.LogicalMax as i64 - y.LogicalMin as i64) as f32,
            });
        }
        if contacts.is_empty() {
            return Err("no usable contact collections".into());
        }
        contacts.sort_by_key(|cap| cap.link);
        Ok(Self {
            preparsed,
            contacts,
            frame: FrameAssembler::default(),
            swipe: SwipeEngine::default(),
        })
    }

    fn value(&self, report: &[u8], page: u16, link: u16, usage: u16) -> Option<u32> {
        let mut value = 0u32;
        (unsafe {
            HidP_GetUsageValue(
                HidP_Input,
                page,
                Some(link),
                usage,
                &mut value,
                PHIDP_PREPARSED_DATA(self.preparsed.as_ptr() as isize),
                report,
            )
        } == HIDP_STATUS_SUCCESS)
            .then_some(value)
    }

    fn active(&self, report: &mut [u8], link: u16) -> bool {
        let mut usages = [0u16; 16];
        let mut length = usages.len() as u32;
        let status = unsafe {
            HidP_GetUsages(
                HidP_Input,
                DIGITIZER,
                Some(link),
                usages.as_mut_ptr(),
                &mut length,
                PHIDP_PREPARSED_DATA(self.preparsed.as_ptr() as isize),
                report,
            )
        };
        status == HIDP_STATUS_SUCCESS
            && usages[..length.min(usages.len() as u32) as usize].contains(&TIP)
            && usages[..length.min(usages.len() as u32) as usize].contains(&CONFIDENCE)
    }

    fn process(&mut self, report: &mut [u8], now: Instant) -> Option<Swipe> {
        let count = self.value(report, DIGITIZER, 0, CONTACT_COUNT)? as usize;
        let scan_time = self.value(report, DIGITIZER, 0, SCAN_TIME)?;
        if count > MAX_CONTACTS {
            self.frame.clear();
            self.swipe.block();
            return None;
        }
        let mut contacts = Vec::with_capacity(self.contacts.len());
        for cap in &self.contacts {
            if !self.active(report, cap.link) {
                continue;
            }
            let (Some(id), Some(x), Some(y)) = (
                self.value(report, DIGITIZER, cap.link, CONTACT_ID),
                self.value(report, DESKTOP, cap.link, X),
                self.value(report, DESKTOP, cap.link, Y),
            ) else {
                self.frame.clear();
                self.swipe.block();
                return None;
            };
            contacts.push(Contact {
                id,
                x: ((x as i64 - cap.x_min as i64) as f32 / cap.x_span).clamp(0.0, 1.0),
                y: ((y as i64 - cap.y_min as i64) as f32 / cap.y_span).clamp(0.0, 1.0),
            });
        }
        let frame = self.frame.push(scan_time, count, contacts)?;
        self.swipe.process(&frame, now)
    }
}

fn has_value(cap: &HIDP_VALUE_CAPS, page: u16, usage: u16) -> bool {
    if cap.UsagePage != page {
        return false;
    }
    if cap.IsRange {
        let range = unsafe { cap.Anonymous.Range };
        range.UsageMin <= usage && usage <= range.UsageMax
    } else {
        unsafe { cap.Anonymous.NotRange.Usage == usage }
    }
}

fn has_button(cap: &HIDP_BUTTON_CAPS, usage: u16) -> bool {
    if cap.UsagePage != DIGITIZER {
        return false;
    }
    if cap.IsRange {
        let range = unsafe { cap.Anonymous.Range };
        range.UsageMin <= usage && usage <= range.UsageMax
    } else {
        unsafe { cap.Anonymous.NotRange.Usage == usage }
    }
}

#[derive(Default)]
struct FrameAssembler {
    scan_time: Option<u32>,
    expected: usize,
    contacts: Vec<Contact>,
}

impl FrameAssembler {
    fn clear(&mut self) {
        self.scan_time = None;
        self.expected = 0;
        self.contacts.clear();
    }

    fn push(
        &mut self,
        scan_time: u32,
        count: usize,
        contacts: Vec<Contact>,
    ) -> Option<Vec<Contact>> {
        if count > 0 && self.scan_time != Some(scan_time) {
            self.clear();
            self.scan_time = Some(scan_time);
            self.expected = count;
        } else if count > 0 && self.expected != count {
            self.clear();
            return None;
        } else if self.scan_time != Some(scan_time) {
            self.clear();
            return Some(Vec::new());
        }
        if self.expected == 0 {
            return Some(Vec::new());
        }
        for contact in contacts {
            if self
                .contacts
                .iter()
                .any(|existing| existing.id == contact.id)
            {
                self.clear();
                return None;
            }
            self.contacts.push(contact);
        }
        if self.contacts.len() > self.expected {
            self.clear();
            return None;
        }
        if self.contacts.len() == self.expected {
            self.scan_time = None;
            self.expected = 0;
            return Some(std::mem::take(&mut self.contacts));
        }
        None
    }
}

#[derive(Default)]
struct SwipeEngine {
    ids: Option<[u32; 3]>,
    origin: (f32, f32),
    last: Option<Instant>,
    fired: bool,
    blocked: bool,
}

impl SwipeEngine {
    fn block(&mut self) {
        self.ids = None;
        self.blocked = true;
    }

    fn process(&mut self, contacts: &[Contact], now: Instant) -> Option<Swipe> {
        if contacts.len() != 3 {
            self.ids = None;
            self.fired = false;
            self.blocked = contacts.len() > 3;
            return None;
        }
        if self.blocked {
            return None;
        }
        let mut ids = [contacts[0].id, contacts[1].id, contacts[2].id];
        ids.sort_unstable();
        if ids[0] == ids[1] || ids[1] == ids[2] {
            self.block();
            return None;
        }
        let center = (
            contacts.iter().map(|contact| contact.x).sum::<f32>() / 3.0,
            contacts.iter().map(|contact| contact.y).sum::<f32>() / 3.0,
        );
        if self.ids.is_some_and(|previous| previous != ids) {
            self.block();
            return None;
        }
        if self.ids.is_none()
            || self
                .last
                .is_some_and(|last| now.duration_since(last) > GESTURE_GAP)
        {
            self.ids = Some(ids);
            self.origin = center;
            self.fired = false;
        }
        self.last = Some(now);
        if self.fired {
            return None;
        }
        let dx = center.0 - self.origin.0;
        let dy = center.1 - self.origin.1;
        let event = if dx.abs() >= SWIPE_TRAVEL && dx.abs() > dy.abs() * AXIS_DOMINANCE {
            if dx > 0.0 {
                Swipe::Right
            } else {
                Swipe::Left
            }
        } else if dy.abs() >= SWIPE_TRAVEL && dy.abs() > dx.abs() * AXIS_DOMINANCE {
            if dy > 0.0 {
                Swipe::Down
            } else {
                Swipe::Up
            }
        } else {
            return None;
        };
        self.fired = true;
        Some(event)
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn touchpad_registration() -> Result<Option<RAWINPUTDEVICE>, String> {
    let mut count = 0u32;
    let stride = size_of::<RAWINPUTDEVICE>() as u32;
    if unsafe { GetRegisteredRawInputDevices(None, &mut count, stride) } == u32::MAX {
        return Err("Raw Input registration query failed".into());
    }
    let mut registrations = vec![RAWINPUTDEVICE::default(); count as usize];
    if count != 0
        && unsafe {
            GetRegisteredRawInputDevices(Some(registrations.as_mut_ptr()), &mut count, stride)
        } == u32::MAX
    {
        return Err("Raw Input registration list failed".into());
    }
    Ok(registrations.into_iter().find(|entry| {
        entry.usUsagePage == DIGITIZER
            && (entry.usUsage == TOUCHPAD
                || (entry.usUsage == 0 && entry.dwFlags.contains(RIDEV_PAGEONLY)))
    }))
}

fn devices() -> Result<HashMap<isize, Device>, String> {
    let mut count = 0u32;
    let stride = size_of::<RAWINPUTDEVICELIST>() as u32;
    if unsafe { GetRawInputDeviceList(None, &mut count, stride) } == u32::MAX {
        return Err("Raw Input device count failed".into());
    }
    let mut list = vec![RAWINPUTDEVICELIST::default(); count as usize];
    if count != 0
        && unsafe { GetRawInputDeviceList(Some(list.as_mut_ptr()), &mut count, stride) } == u32::MAX
    {
        return Err("Raw Input device list failed".into());
    }
    let mut result = HashMap::new();
    for entry in list.into_iter().take(count as usize) {
        if entry.dwType == RIM_TYPEHID {
            if let Ok(device) = Device::open(entry.hDevice) {
                result.insert(entry.hDevice.0 as isize, device);
            }
        }
    }
    Ok(result)
}

pub(crate) struct RawTouchpad {
    window: HWND,
    devices: HashMap<isize, Device>,
    ignored_devices: HashSet<isize>,
    storage: Vec<u64>,
}

impl RawTouchpad {
    pub(crate) fn start() -> Result<Self, String> {
        if touchpad_registration()?.is_some() {
            return Err("another touchpad Raw Input target is registered in this process".into());
        }
        let devices = devices()?;
        if devices.is_empty() {
            return Err("no compatible Precision Touchpad Raw Input collection".into());
        }
        let class = w!("LeopardWMRawTouchpad");
        let window_class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            lpszClassName: class,
            ..Default::default()
        };
        if unsafe { RegisterClassW(&window_class) } == 0 {
            return Err("Raw Input window class registration failed".into());
        }
        let window = match unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class,
                w!("LeopardWM Raw Touchpad"),
                WINDOW_STYLE::default(),
                0,
                0,
                0,
                0,
                None,
                None,
                None,
                None,
            )
        } {
            Ok(window) => window,
            Err(error) => {
                let _ = unsafe { UnregisterClassW(class, None) };
                return Err(format!("Raw Input window creation failed: {error}"));
            }
        };
        let registration = RAWINPUTDEVICE {
            usUsagePage: DIGITIZER,
            usUsage: TOUCHPAD,
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: window,
        };
        if let Err(error) =
            unsafe { RegisterRawInputDevices(&[registration], size_of::<RAWINPUTDEVICE>() as u32) }
        {
            let _ = unsafe { DestroyWindow(window) };
            let _ = unsafe { UnregisterClassW(class, None) };
            return Err(format!("touchpad Raw Input registration failed: {error}"));
        }
        Ok(Self {
            window,
            devices,
            ignored_devices: HashSet::new(),
            storage: Vec::new(),
        })
    }

    pub(crate) fn process(&mut self, message: &MSG) -> Option<Swipe> {
        if message.message != WM_INPUT || message.hwnd != self.window {
            return None;
        }
        let mut bytes = 0u32;
        let header_bytes = size_of::<RAWINPUTHEADER>() as u32;
        let handle = HRAWINPUT(message.lParam.0 as *mut _);
        if unsafe { GetRawInputData(handle, RID_INPUT, None, &mut bytes, header_bytes) } == u32::MAX
            || (bytes as usize) < size_of::<RAWINPUTHEADER>() + 8
            || bytes as usize > MAX_INPUT_BYTES
        {
            return None;
        }
        self.storage
            .resize((bytes as usize).max(size_of::<RAWINPUT>()).div_ceil(8), 0);
        let copied = unsafe {
            GetRawInputData(
                handle,
                RID_INPUT,
                Some(self.storage.as_mut_ptr().cast()),
                &mut bytes,
                header_bytes,
            )
        };
        if copied == u32::MAX
            || (copied as usize) < size_of::<RAWINPUTHEADER>() + 8
            || copied as usize > bytes as usize
        {
            return None;
        }
        let raw = unsafe { &*self.storage.as_ptr().cast::<RAWINPUT>() };
        if raw.header.dwType != RIM_TYPEHID.0 {
            return None;
        }
        let key = raw.header.hDevice.0 as isize;
        if self.ignored_devices.contains(&key) {
            return None;
        }
        if let std::collections::hash_map::Entry::Vacant(entry) = self.devices.entry(key) {
            match Device::open(raw.header.hDevice) {
                Ok(device) => {
                    entry.insert(device);
                }
                Err(_) => {
                    self.ignored_devices.insert(key);
                    return None;
                }
            }
        }
        let device = self.devices.get_mut(&key)?;
        let hid = unsafe { raw.data.hid };
        let report_bytes = hid.dwSizeHid as usize;
        let start = size_of::<RAWINPUTHEADER>() + 8;
        let end = report_bytes
            .checked_mul(hid.dwCount as usize)?
            .checked_add(start)?;
        if report_bytes == 0 || end > bytes as usize {
            return None;
        }
        let data = unsafe {
            std::slice::from_raw_parts_mut(
                self.storage.as_mut_ptr().cast::<u8>().add(start),
                end - start,
            )
        };
        let mut event = None;
        for report in data.chunks_exact_mut(report_bytes) {
            if let Some(swipe) = device.process(report, Instant::now()) {
                event = Some(swipe);
            }
        }
        event
    }
}

impl Drop for RawTouchpad {
    fn drop(&mut self) {
        if touchpad_registration()
            .ok()
            .flatten()
            .is_some_and(|entry| entry.hwndTarget == self.window)
        {
            let removal = RAWINPUTDEVICE {
                usUsagePage: DIGITIZER,
                usUsage: TOUCHPAD,
                dwFlags: RIDEV_REMOVE,
                hwndTarget: HWND::default(),
            };
            let _ =
                unsafe { RegisterRawInputDevices(&[removal], size_of::<RAWINPUTDEVICE>() as u32) };
        }
        let _ = unsafe { DestroyWindow(self.window) };
        let _ = unsafe { UnregisterClassW(w!("LeopardWMRawTouchpad"), None) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_usage_matches_page_and_range() {
        let mut value = HIDP_VALUE_CAPS {
            UsagePage: DIGITIZER,
            IsRange: true,
            ..Default::default()
        };
        value.Anonymous.Range.UsageMin = CONTACT_ID;
        value.Anonymous.Range.UsageMax = CONTACT_ID + 2;
        assert!(has_value(&value, DIGITIZER, CONTACT_ID));
        assert!(!has_value(&value, DESKTOP, CONTACT_ID));
        assert!(!has_value(&value, DIGITIZER, CONTACT_COUNT));

        let mut button = HIDP_BUTTON_CAPS {
            UsagePage: DIGITIZER,
            ..Default::default()
        };
        button.Anonymous.NotRange.Usage = TIP;
        assert!(has_button(&button, TIP));
        assert!(!has_button(&button, CONFIDENCE));
    }

    fn contacts(x: f32, y: f32) -> Vec<Contact> {
        (0..3).map(|id| Contact { id, x, y }).collect()
    }

    #[test]
    fn assembles_split_three_contact_frame() {
        let mut frame = FrameAssembler::default();
        assert!(frame
            .push(10, 3, contacts(0.2, 0.2)[..2].to_vec())
            .is_none());
        assert_eq!(
            frame
                .push(10, 0, contacts(0.2, 0.2)[2..].to_vec())
                .unwrap()
                .iter()
                .map(|contact| (contact.id, contact.x, contact.y))
                .collect::<Vec<_>>(),
            [(0, 0.2, 0.2), (1, 0.2, 0.2), (2, 0.2, 0.2)]
        );
        assert!(frame.push(11, 0, Vec::new()).unwrap().is_empty());

        assert!(frame
            .push(12, 3, contacts(0.2, 0.2)[..2].to_vec())
            .is_none());
        assert_eq!(
            frame
                .push(12, 3, contacts(0.2, 0.2)[2..].to_vec())
                .unwrap()
                .iter()
                .map(|contact| (contact.id, contact.x, contact.y))
                .collect::<Vec<_>>(),
            [(0, 0.2, 0.2), (1, 0.2, 0.2), (2, 0.2, 0.2)]
        );
    }

    #[test]
    fn incomplete_or_duplicate_frames_do_not_complete() {
        let mut frame = FrameAssembler::default();
        assert!(frame
            .push(10, 3, contacts(0.2, 0.2)[..1].to_vec())
            .is_none());
        assert!(frame
            .push(11, 0, contacts(0.3, 0.3)[1..].to_vec())
            .unwrap()
            .is_empty());
        assert!(frame
            .push(12, 3, contacts(0.2, 0.2)[..2].to_vec())
            .is_none());
        assert!(frame
            .push(12, 0, contacts(0.2, 0.2)[..1].to_vec())
            .is_none());
    }

    #[test]
    fn one_swipe_per_three_contact_episode() {
        let mut engine = SwipeEngine::default();
        let now = Instant::now();
        assert_eq!(engine.process(&contacts(0.2, 0.5), now), None);
        assert_eq!(engine.process(&contacts(0.4, 0.5), now), Some(Swipe::Right));
        assert_eq!(engine.process(&contacts(0.6, 0.5), now), None);
        engine.process(&[], now);
        assert_eq!(engine.process(&contacts(0.6, 0.5), now), None);
        assert_eq!(engine.process(&contacts(0.3, 0.5), now), Some(Swipe::Left));
    }

    #[test]
    fn direction_and_interruption_rules() {
        let now = Instant::now();
        for (end, expected) in [
            ((0.2, 0.5), Swipe::Left),
            ((0.8, 0.5), Swipe::Right),
            ((0.5, 0.2), Swipe::Up),
            ((0.5, 0.8), Swipe::Down),
        ] {
            let mut engine = SwipeEngine::default();
            engine.process(&contacts(0.5, 0.5), now);
            assert_eq!(engine.process(&contacts(end.0, end.1), now), Some(expected));
        }
        let mut engine = SwipeEngine::default();
        engine.process(&contacts(0.5, 0.5), now);
        assert_eq!(engine.process(&contacts(0.7, 0.7), now), None);
        engine.process(&contacts(0.5, 0.5)[..2], now);
        assert_eq!(engine.process(&contacts(0.8, 0.5), now), None);
    }

    #[test]
    fn changed_contact_identity_or_fourth_finger_cancels_episode() {
        let now = Instant::now();
        let mut engine = SwipeEngine::default();
        engine.process(&contacts(0.2, 0.5), now);
        let mut replaced = contacts(0.4, 0.5);
        replaced[0].id = 9;
        assert_eq!(engine.process(&replaced, now), None);
        assert_eq!(engine.process(&contacts(0.7, 0.5), now), None);
        engine.process(&[], now);
        engine.process(&contacts(0.2, 0.5), now);
        let mut four = contacts(0.3, 0.5);
        four.push(Contact {
            id: 9,
            x: 0.3,
            y: 0.5,
        });
        assert_eq!(engine.process(&four, now), None);
        assert_eq!(engine.process(&contacts(0.7, 0.5), now), None);
    }

    #[test]
    fn stale_three_contact_motion_starts_a_new_origin() {
        let now = Instant::now();
        let mut engine = SwipeEngine::default();
        engine.process(&contacts(0.2, 0.5), now);
        assert_eq!(
            engine.process(
                &contacts(0.8, 0.5),
                now + GESTURE_GAP + Duration::from_millis(1)
            ),
            None
        );
    }
}
