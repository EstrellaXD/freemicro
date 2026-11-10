//! IOKit HID binding: hand-written FFI to IOKit and CoreFoundation. macOS
//! only. Matching on VendorID + ProductID, USB preferred over BLE. Opened
//! non-exclusively, as every other host does.
//!
//! A Codex Micro vends one IOHIDDevice whose primary usage is the keyboard and
//! which only contains the 0xFF00 collection, so matching only on the usage
//! page would miss it; a Creator Micro 2 vends one per USB interface, so the
//! 0xFF00 one is preferred (`service_rank`). The product string and collection count differ by transport, so
//! they are not matched either.
//!
//! The input-report callback is registered and scheduled on the opening
//! thread's run loop once, at open, so a reply that arrives between `send`
//! and `poll` is queued, never missed. The callback only feeds a
//! [`FrameDecoder`]; it never calls back into the rest of the crate.

use std::ffi::{c_char, c_void, CStr};
use std::ptr;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::frame::{frame_message, to_ascii_json, FrameDecoder};
use super::{DeviceError, PadInfo, PadIo, Transport, PRODUCT_IDS, REPORT_ID, VENDOR_ID};

type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFRunLoopRef = *mut c_void;
type IOHIDDeviceRef = *mut c_void;
type IoObject = u32;
type ReportCallback = extern "C" fn(*mut c_void, i32, *mut c_void, u32, u32, *mut u8, isize);

const UTF8: u32 = 0x0800_0100; // kCFStringEncodingUTF8
const NUMBER_SINT32: isize = 3; // kCFNumberSInt32Type
const REPORT_TYPE_OUTPUT: u32 = 1; // kIOHIDReportTypeOutput
const RUN_FINISHED: i32 = 1; // kCFRunLoopRunFinished
/// Input buffer: the report id byte plus 63 data bytes.
const INPUT_BYTES: usize = 64;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFRunLoopDefaultMode: CFStringRef;
    fn CFRelease(cf: CFTypeRef);
    fn CFStringCreateWithCString(alloc: CFTypeRef, text: *const c_char, enc: u32) -> CFStringRef;
    fn CFStringGetCString(s: CFStringRef, buf: *mut c_char, size: isize, enc: u32) -> u8;
    fn CFNumberGetValue(number: CFTypeRef, kind: isize, out: *mut c_void) -> u8;
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopRunInMode(mode: CFStringRef, seconds: f64, return_after_source: u8) -> i32;
}

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOServiceMatching(name: *const c_char) -> *mut c_void;
    fn IOServiceGetMatchingServices(port: u32, matching: *mut c_void, it: *mut IoObject) -> i32;
    fn IOIteratorNext(it: IoObject) -> IoObject;
    fn IOObjectRelease(object: IoObject) -> i32;
    fn IORegistryEntryGetRegistryEntryID(entry: IoObject, id: *mut u64) -> i32;
    fn IORegistryEntryCreateCFProperty(
        entry: IoObject,
        key: CFStringRef,
        alloc: CFTypeRef,
        options: u32,
    ) -> CFTypeRef;
    fn IOHIDDeviceCreate(alloc: CFTypeRef, service: IoObject) -> IOHIDDeviceRef;
    fn IOHIDDeviceOpen(device: IOHIDDeviceRef, options: u32) -> i32;
    fn IOHIDRequestAccess(request_type: u32) -> u8;
    fn IOHIDDeviceClose(device: IOHIDDeviceRef, options: u32) -> i32;
    fn IOHIDDeviceScheduleWithRunLoop(device: IOHIDDeviceRef, rl: CFRunLoopRef, mode: CFStringRef);
    fn IOHIDDeviceUnscheduleFromRunLoop(
        device: IOHIDDeviceRef,
        rl: CFRunLoopRef,
        mode: CFStringRef,
    );
    fn IOHIDDeviceRegisterInputReportCallback(
        device: IOHIDDeviceRef,
        report: *mut u8,
        length: isize,
        callback: Option<ReportCallback>,
        context: *mut c_void,
    );
    fn IOHIDDeviceSetReport(
        device: IOHIDDeviceRef,
        kind: u32,
        report_id: isize,
        report: *const u8,
        length: isize,
    ) -> i32;
}

/// What the input callback owns. Separate heap allocation from the input
/// buffer, so the callback's `&mut` never aliases the report bytes.
#[derive(Default)]
struct Inbox {
    decoder: FrameDecoder,
    queue: Vec<Value>,
}

extern "C" fn on_report(
    context: *mut c_void,
    _result: i32,
    _sender: *mut c_void,
    _kind: u32,
    _report_id: u32,
    report: *mut u8,
    length: isize,
) {
    if context.is_null() || report.is_null() {
        return;
    }
    // SAFETY: `context` is the `Inbox` HidPad allocated and keeps alive until
    // after this callback is unregistered; it runs only inside `poll`'s
    // CFRunLoopRunInMode, when no other reference to the inbox is live.
    let inbox = unsafe { &mut *(context as *mut Inbox) };
    let n = (length.max(0) as usize).min(INPUT_BYTES);
    // SAFETY: IOKit hands a buffer of at least `length` bytes (our own
    // INPUT_BYTES buffer); `n` is clamped to both.
    let raw = unsafe { std::slice::from_raw_parts(report, n) };
    let messages = inbox.decoder.feed(raw);
    inbox.queue.extend(messages);
}

/// An open IOHIDDeviceRef plus its decoder and input buffer. Not `Send`.
pub struct HidPad {
    info: PadInfo,
    service: u64,
    device: IOHIDDeviceRef,
    run_loop: CFRunLoopRef,
    inbox: *mut Inbox,
    inbuf: *mut [u8; INPUT_BYTES],
    /// Keeps the handle on the thread that opened it (raw IOKit refs).
    _not_send: std::marker::PhantomData<*const ()>,
}

impl PadIo for HidPad {
    fn info(&self) -> &PadInfo {
        &self.info
    }

    fn service(&self) -> u64 {
        self.service
    }

    fn send(&mut self, message: &Value) -> Result<(), DeviceError> {
        let payload = to_ascii_json(message);
        for report in frame_message(&payload, self.info.framing()) {
            // SAFETY: `device` is open until Drop; the buffer outlives the call.
            let result = unsafe {
                IOHIDDeviceSetReport(
                    self.device,
                    REPORT_TYPE_OUTPUT,
                    REPORT_ID as isize,
                    report.as_ptr(),
                    report.len() as isize,
                )
            };
            if result != 0 {
                return Err(DeviceError(format!(
                    "IOHIDDeviceSetReport failed: 0x{:08X}",
                    result as u32
                )));
            }
        }
        Ok(())
    }

    fn poll(&mut self, timeout: Duration) -> Result<Vec<Value>, DeviceError> {
        let deadline = Instant::now() + timeout;
        loop {
            // SAFETY: the callback is not running (we are not inside the run
            // loop), so this is the only reference to the inbox.
            let queue = unsafe { &mut (*self.inbox).queue };
            if !queue.is_empty() {
                return Ok(std::mem::take(queue));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(Vec::new());
            }
            // SAFETY: called on the thread that scheduled the device.
            let result =
                unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, left.as_secs_f64(), 1) };
            if result == RUN_FINISHED {
                // No source left in the mode: the device went away.
                return Err(DeviceError("the pad's run loop source is gone".into()));
            }
        }
    }
}

impl Drop for HidPad {
    /// Unregister the callback, unschedule, close and release the device.
    /// Unregister first: IOKit must not hold our pointers once they are freed.
    fn drop(&mut self) {
        // SAFETY: all handles were created in `open_hid` and are released
        // exactly once here, on the opening thread (HidPad is !Send).
        unsafe {
            IOHIDDeviceRegisterInputReportCallback(
                self.device,
                (*self.inbuf).as_mut_ptr(),
                INPUT_BYTES as isize,
                None,
                ptr::null_mut(),
            );
            IOHIDDeviceUnscheduleFromRunLoop(self.device, self.run_loop, kCFRunLoopDefaultMode);
            IOHIDDeviceClose(self.device, 0);
            CFRelease(self.device);
            drop(Box::from_raw(self.inbox));
            drop(Box::from_raw(self.inbuf));
        }
    }
}

/// Read a registry property of `service` with `read`, then release it.
fn property<T>(service: IoObject, key: &CStr, read: impl FnOnce(CFTypeRef) -> T) -> Option<T> {
    // SAFETY: plain CF/IOKit calls; every created object is released here.
    unsafe {
        let name = CFStringCreateWithCString(ptr::null(), key.as_ptr(), UTF8);
        let value = IORegistryEntryCreateCFProperty(service, name, ptr::null(), 0);
        CFRelease(name);
        if value.is_null() {
            return None;
        }
        let out = read(value);
        CFRelease(value);
        Some(out)
    }
}

fn prop_int(service: IoObject, key: &CStr) -> Option<i32> {
    property(service, key, |value| {
        let mut out: i32 = 0;
        // SAFETY: VendorID / ProductID are CFNumbers; `out` is an SInt32.
        let ok = unsafe { CFNumberGetValue(value, NUMBER_SINT32, &mut out as *mut i32 as _) };
        (ok != 0).then_some(out)
    })
    .flatten()
}

fn prop_str(service: IoObject, key: &CStr) -> Option<String> {
    property(service, key, |value| {
        let mut buf = [0 as c_char; 256];
        // SAFETY: `Transport` is a CFString; the buffer size is passed.
        let ok = unsafe { CFStringGetCString(value, buf.as_mut_ptr(), buf.len() as isize, UTF8) };
        // SAFETY: on success the buffer holds a NUL-terminated string.
        (ok != 0).then(|| {
            unsafe { CStr::from_ptr(buf.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        })
    })
    .flatten()
}

/// The pad's IOHIDDevice service (retained; the caller releases it), its
/// transport and product id. USB wins when the pad is on cable and BLE.
fn matching_service() -> Option<(IoObject, Transport, u16)> {
    let mut iterator: IoObject = 0;
    // SAFETY: IOServiceGetMatchingServices consumes the matching dictionary.
    let kr = unsafe {
        IOServiceGetMatchingServices(0, IOServiceMatching(c"IOHIDDevice".as_ptr()), &mut iterator)
    };
    if kr != 0 {
        return None;
    }
    let mut best: Option<(IoObject, Transport, u16, (bool, bool))> = None;
    loop {
        // SAFETY: `iterator` is valid until released below.
        let candidate = unsafe { IOIteratorNext(iterator) };
        if candidate == 0 {
            break;
        }
        let product_id = prop_int(candidate, c"ProductID")
            .and_then(|id| u16::try_from(id).ok())
            .filter(|id| PRODUCT_IDS.contains(id));
        let vendor_ok = prop_int(candidate, c"VendorID") == Some(i32::from(VENDOR_ID));
        let (Some(product_id), true) = (product_id, vendor_ok) else {
            // SAFETY: we own `candidate`.
            unsafe { IOObjectRelease(candidate) };
            continue;
        };
        let transport = Transport::from_iokit(
            &prop_str(candidate, c"Transport").unwrap_or_else(|| "unknown".into()),
        );
        let rank = service_rank(&transport, prop_int(candidate, c"PrimaryUsagePage"));
        if best.as_ref().is_none_or(|(_, _, _, old)| rank > *old) {
            if let Some((old, ..)) = best {
                // SAFETY: we own the previous best.
                unsafe { IOObjectRelease(old) };
            }
            best = Some((candidate, transport, product_id, rank));
        } else {
            // SAFETY: we own `candidate`.
            unsafe { IOObjectRelease(candidate) };
        }
    }
    // SAFETY: we own the iterator.
    unsafe { IOObjectRelease(iterator) };
    best.map(|(service, transport, product_id, _)| (service, transport, product_id))
}

/// Higher is better: USB first, then the vendor (0xFF00) interface. A Codex
/// Micro vends one service that only contains the vendor collection; a
/// Creator Micro 2 vends one per USB interface, and only the vendor one answers.
fn service_rank(transport: &Transport, usage_page: Option<i32>) -> (bool, bool) {
    (
        *transport == Transport::Usb,
        usage_page == Some(super::USAGE_PAGE_VENDOR as i32),
    )
}

/// Registry entry id of `service`: stable while the service lives, never
/// reused, so it tells a reconnected pad from the one that was opened.
fn entry_id(service: IoObject) -> u64 {
    let mut id = 0;
    // SAFETY: `service` is a valid registry entry; `id` is a u64 out-param.
    unsafe { IORegistryEntryGetRegistryEntryID(service, &mut id) };
    id
}

/// Entry id of the best matching service, without opening it.
pub fn service_present() -> Option<u64> {
    let (service, ..) = matching_service()?;
    let id = entry_id(service);
    // SAFETY: we own `service`.
    unsafe { IOObjectRelease(service) };
    Some(id)
}

/// Open the best matching service. `Ok(None)` if absent or refused.
pub fn open_hid() -> Result<Option<HidPad>, DeviceError> {
    let Some((service, transport, product_id)) = matching_service() else {
        return Ok(None);
    };
    // SAFETY: plain IOKit calls on handles we own; on every failure path the
    // created device is released.
    unsafe {
        let service_id = entry_id(service);
        let device = IOHIDDeviceCreate(ptr::null(), service);
        IOObjectRelease(service);
        if device.is_null() {
            return Ok(None);
        }
        // Non-exclusive (options 0). Failure is almost always a missing
        // Input Monitoring grant.
        let kr = IOHIDDeviceOpen(device, 0);
        if kr != 0 {
            CFRelease(device);
            return Err(DeviceError(if kr as u32 == 0xE000_02E2 {
                // Ask once per process: macOS shows its own prompt for this binary.
                static ASKED: std::sync::Once = std::sync::Once::new();
                ASKED.call_once(|| {
                    IOHIDRequestAccess(1); // kIOHIDRequestTypeListenEvent
                });
                "macOS refused to open the pad (0xE00002E2): allow Input Monitoring".into()
            } else {
                format!("IOHIDDeviceOpen failed: 0x{:08X}", kr as u32)
            }));
        }
        let pad = HidPad {
            info: PadInfo {
                transport,
                product_id,
            },
            service: service_id,
            device,
            run_loop: CFRunLoopGetCurrent(),
            inbox: Box::into_raw(Box::default()),
            inbuf: Box::into_raw(Box::new([0u8; INPUT_BYTES])),
            _not_send: std::marker::PhantomData,
        };
        IOHIDDeviceRegisterInputReportCallback(
            device,
            (*pad.inbuf).as_mut_ptr(),
            INPUT_BYTES as isize,
            Some(on_report),
            pad.inbox as *mut c_void,
        );
        IOHIDDeviceScheduleWithRunLoop(device, pad.run_loop, kCFRunLoopDefaultMode);
        Ok(Some(pad))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_rank_prefers_usb_then_the_vendor_interface() {
        let ble = Transport::from_iokit("Bluetooth Low Energy");
        let vendor = Some(0xFF00);
        // Creator Micro 2 over USB: keyboard, consumer, vendor.
        assert!(service_rank(&Transport::Usb, vendor) > service_rank(&Transport::Usb, Some(12)));
        assert!(service_rank(&Transport::Usb, vendor) > service_rank(&Transport::Usb, Some(1)));
        // USB still beats BLE, whatever the usage page.
        assert!(service_rank(&Transport::Usb, Some(1)) > service_rank(&ble, vendor));
    }
}
