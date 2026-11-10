//! The pad's vendor HID channel: discovery, framing, JSON-RPC requests and
//! the on-pad filesystem. Port of `device/codex_micro.py`,
//! `device/__init__.py` and `device/padfs.py`.
//!
//! Owner: device. Leaf module. Everything above it talks to the pad only
//! through [`PadIo`], and only from the pad-owner thread (see
//! ARCHITECTURE.md). Tests use [`fake::FakePad`]; nothing in a test may call
//! [`open`].

pub mod fake;
pub mod frame;
pub mod iokit;
pub mod padfs;

use std::fmt;
use std::time::{Duration, Instant};

use serde_json::Value;

pub const VENDOR_ID: u16 = 0x303A;
/// Codex Micro.
pub const PRODUCT_ID_CODEX_MICRO: u16 = 0x8360;
/// Creator Micro 2 (Work Louder registers both).
pub const CREATOR_MICRO_2_PRODUCT_IDS: [u16; 2] = [0x8297, 0x8298];
pub const PRODUCT_IDS: [u16; 3] = [0x8360, 0x8297, 0x8298];
pub const USAGE_PAGE_VENDOR: u32 = 0xFF00;
/// Vendor collection report id, input and output.
pub const REPORT_ID: u8 = 6;

/// Device -> host notifications.
pub const EVENT_KEY: &str = "v.oai.hid";
pub const EVENT_JOYSTICK: &str = "v.oai.rad";

/// Any non-empty value makes the process behave as if no pad were attached.
pub const ENV_NO_DEVICE: &str = "FREEMICRO_NO_DEVICE";

/// Request ids must stay below this (firmware / Input convention).
pub const MAX_REQUEST_ID: u32 = 999;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    Usb,
    Ble,
    Other(String),
}

impl Transport {
    /// From the IOHIDDevice `Transport` property (`"USB"`,
    /// `"Bluetooth Low Energy"`).
    pub fn from_iokit(text: &str) -> Transport {
        match text {
            "USB" => Transport::Usb,
            "Bluetooth Low Energy" => Transport::Ble,
            other => Transport::Other(other.to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Transport::Usb => "USB",
            Transport::Ble => "Bluetooth Low Energy",
            Transport::Other(text) => text,
        }
    }
}

/// How writes are framed. Codex Micro: USB 63-byte `[02][len][json]` + CRLF,
/// BLE 64-byte `[06][02][len][json]` + CRLF. Creator Micro 2: always the
/// 64-byte prefixed form and no CRLF (CRLF gets `400 EmptyInput`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Framing {
    /// 64-byte buffer with the report id repeated as byte 0.
    pub prefixed: bool,
    /// Append `\r\n` to each message.
    pub crlf: bool,
}

/// What was opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PadInfo {
    pub transport: Transport,
    pub product_id: u16,
}

impl PadInfo {
    pub fn is_creator_micro_2(&self) -> bool {
        CREATOR_MICRO_2_PRODUCT_IDS.contains(&self.product_id)
    }

    pub fn framing(&self) -> Framing {
        if self.is_creator_micro_2() {
            Framing {
                prefixed: true,
                crlf: false,
            }
        } else {
            Framing {
                prefixed: self.transport == Transport::Ble,
                crlf: true,
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceError(pub String);

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DeviceError {}

/// One open pad. Exactly one instance exists per process, owned by the
/// pad-owner thread. Never shared, never cloned, never sent across threads.
pub trait PadIo {
    fn info(&self) -> &PadInfo;

    /// Identity of the opened service (IOKit registry entry id). The stream
    /// loop compares it with [`present`]: a pad that came back on another
    /// service (BLE, then the cable again) must be reopened, not kept.
    fn service(&self) -> u64 {
        0
    }

    /// Serialise (compact, ASCII-escaped JSON), frame and write one message.
    /// Fire and forget: success means the OS took the reports, not that the
    /// pad acted on them.
    fn send(&mut self, message: &Value) -> Result<(), DeviceError>;

    /// Wait up to `timeout` for input; return the messages decoded meanwhile
    /// (possibly none). Returns early once at least one message is decoded.
    /// The IOKit impl pumps the current thread's CFRunLoop here, so the
    /// caller must be the main thread.
    fn poll(&mut self, timeout: Duration) -> Result<Vec<Value>, DeviceError>;
}

/// True when `FREEMICRO_NO_DEVICE` is set non-empty.
pub fn no_device() -> bool {
    std::env::var_os(ENV_NO_DEVICE).is_some_and(|v| !v.is_empty())
}

/// Service id of the pad that [`open`] would pick (USB preferred), without
/// opening it. `None` when absent or under `FREEMICRO_NO_DEVICE`. Cheap
/// enough for the once-a-second presence check.
pub fn present() -> Option<u64> {
    if no_device() {
        return None;
    }
    iokit::service_present()
}

/// Open the pad's vendor channel. `Ok(None)` when absent, not permitted
/// (Input Monitoring) or under `FREEMICRO_NO_DEVICE`.
pub fn open() -> Result<Option<Box<dyn PadIo>>, DeviceError> {
    if no_device() {
        return Ok(None);
    }
    Ok(iokit::open_hid()?.map(|pad| Box::new(pad) as Box<dyn PadIo>))
}

/// A fire-and-forget `{"m": method, "p": params}` (no id: `v.oai.*` 404 with
/// one).
pub fn notification(method: &str, params: Value) -> Value {
    serde_json::json!({ "m": method, "p": params })
}

/// One request that expects a reply.
#[derive(Debug, Clone)]
pub struct Request<'a> {
    pub method: &'a str,
    pub params: Option<Value>,
    /// Below [`MAX_REQUEST_ID`].
    pub id: u32,
    pub timeout: Duration,
    /// Accept only the reply carrying `id` (file writes). Otherwise the first
    /// message with `result` or `error` counts.
    pub match_id: bool,
    /// Long envelope `{"method","params","id"}`; required for `fs.writebin`.
    /// Otherwise `{"m","p","id"}`.
    pub standard: bool,
}

/// Send `req` and wait for its reply on the same thread. Every other
/// message decoded meanwhile (key events) goes to `stray`, in order, so a
/// request issued from inside the run loop loses no input. `Ok(None)` on
/// timeout.
pub fn request(
    io: &mut dyn PadIo,
    req: &Request<'_>,
    stray: &mut dyn FnMut(Value),
) -> Result<Option<Value>, DeviceError> {
    let (method_key, params_key) = if req.standard {
        ("method", "params")
    } else {
        ("m", "p")
    };
    let mut message = serde_json::Map::new();
    message.insert(method_key.into(), req.method.into());
    message.insert("id".into(), req.id.into());
    if let Some(params) = &req.params {
        message.insert(params_key.into(), params.clone());
    }
    // The input callback is registered at open, so a reply that arrives
    // before the first poll is already queued.
    io.send(&Value::Object(message))?;

    let deadline = Instant::now() + req.timeout;
    let mut reply = None;
    while reply.is_none() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        for msg in io.poll(left)? {
            let ours = msg.get("id").and_then(Value::as_u64) == Some(u64::from(req.id))
                || (!req.match_id && (msg.get("result").is_some() || msg.get("error").is_some()));
            if reply.is_none() && ours {
                reply = Some(msg);
            } else {
                stray(msg);
            }
        }
    }
    Ok(reply)
}

/// `device.status` round trip, the only proof the channel works. Returns its
/// `result` object (`version`, `profile_index`, `layer_index` 1-based,
/// `battery`, `is_charging`). Matches on `id` (firmware 0.6.3 echoes it), so an
/// LED ack that arrives first is not taken for the status.
pub fn status(
    io: &mut dyn PadIo,
    id: u32,
    timeout: Duration,
    stray: &mut dyn FnMut(Value),
) -> Result<Option<Value>, DeviceError> {
    let req = Request {
        method: "device.status",
        params: None,
        id,
        timeout,
        match_id: true,
        standard: false,
    };
    Ok(
        request(io, &req, stray)?.map(|reply| match reply.get("result") {
            Some(result @ Value::Object(_)) => result.clone(),
            _ => reply,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::fake::FakePad;
    use super::*;
    use serde_json::json;

    fn key(act: u8) -> Value {
        json!({"m": EVENT_KEY, "p": {"k": "AG02", "act": act}})
    }

    fn req(match_id: bool, standard: bool) -> Request<'static> {
        Request {
            method: "fs.list",
            params: Some(json!({"checksum": true})),
            id: 530,
            timeout: Duration::from_millis(20),
            match_id,
            standard,
        }
    }

    #[test]
    fn request_uses_the_envelope_for_standard() {
        let cases = [
            (
                false,
                json!({"m": "fs.list", "id": 530, "p": {"checksum": true}}),
            ),
            (
                true,
                json!({"method": "fs.list", "id": 530, "params": {"checksum": true}}),
            ),
        ];
        for (standard, expected) in cases {
            let mut pad = FakePad::creator_micro_2();
            let reply = request(&mut pad, &req(false, standard), &mut |_| {}).unwrap();
            assert_eq!(reply, None);
            assert_eq!(pad.sent, vec![expected]);
        }
    }

    #[test]
    fn request_routes_everything_but_the_reply_to_stray_in_order() {
        // hw.log / keys.log: an id-less LED ack, a key event and another
        // client's reply arrive before ours.
        let led_ack = json!({"result": {"ok": 1}, "id": null, "method": "v.oai.thstatus"});
        let other_id = json!({"result": {"ok": 1}, "id": 959, "method": "v.oai.rgbcfg"});
        let ours = json!({"result": [], "id": 530, "method": "fs.list"});
        let incoming = vec![
            led_ack.clone(),
            key(1),
            other_id.clone(),
            ours.clone(),
            key(0),
        ];
        // match_id: only id 530 counts. Otherwise the first result wins.
        for (match_id, expected) in [(true, ours.clone()), (false, led_ack.clone())] {
            let batch = incoming.clone();
            let mut pad = FakePad::creator_micro_2().respond(move |_| batch.clone());
            let mut strays = Vec::new();
            let reply = request(&mut pad, &req(match_id, true), &mut |m| strays.push(m)).unwrap();
            assert_eq!(reply, Some(expected.clone()));
            let mut rest = incoming.clone();
            rest.retain(|m| *m != expected);
            assert_eq!(strays, rest);
        }
    }

    #[test]
    fn request_propagates_a_dropped_pad() {
        let mut pad = FakePad::creator_micro_2();
        pad.fail = Some(DeviceError("gone".into()));
        let err = request(&mut pad, &req(true, true), &mut |_| {}).unwrap_err();
        assert_eq!(err, DeviceError("gone".into()));
    }

    #[test]
    fn status_returns_the_result_object_else_the_reply() {
        // hw2.log device.status reply.
        let full = json!({
            "result": {"version": "0.6.3", "profile_index": 0, "layer_index": 1, "battery": 99, "is_charging": true},
            "id": 102,
            "method": "device.status"
        });
        let odd = json!({"result": "busy", "id": 102});
        // An LED ack without our id (from an earlier write) is not the reply.
        let ack = json!({"result": {"ok": 1}, "id": null});
        for (reply, expected) in [(full.clone(), full["result"].clone()), (odd.clone(), odd)] {
            let ack = ack.clone();
            let mut pad =
                FakePad::creator_micro_2().respond(move |_| vec![ack.clone(), reply.clone()]);
            let got = status(&mut pad, 102, Duration::from_millis(20), &mut |_| {}).unwrap();
            assert_eq!(got, Some(expected));
            assert_eq!(pad.sent, vec![json!({"m": "device.status", "id": 102})]);
        }
    }

    #[test]
    fn framing_follows_product_id_then_transport() {
        let cases = [
            (PRODUCT_ID_CODEX_MICRO, Transport::Usb, false, true),
            (PRODUCT_ID_CODEX_MICRO, Transport::Ble, true, true),
            (0x8297, Transport::Usb, true, false),
            (0x8298, Transport::Usb, true, false),
            (0x8298, Transport::Ble, true, false),
        ];
        for (product_id, transport, prefixed, crlf) in cases {
            let info = PadInfo {
                transport,
                product_id,
            };
            assert_eq!(info.framing(), Framing { prefixed, crlf });
        }
    }
}
