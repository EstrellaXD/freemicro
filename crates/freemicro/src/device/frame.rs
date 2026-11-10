//! Report framing and reassembly. Pure; no IOKit. Port of `frame_message`
//! and `FrameDecoder`.

use std::fmt::Write as _;

use serde_json::Value;

use super::{Framing, REPORT_ID};

pub const REPORT_BYTES: usize = 63;
pub const REPORT_BYTES_BLE: usize = 64;
pub const OPCODE_DATA: u8 = 0x02;
/// Data bytes per report, the same for both layouts.
pub const MAX_CHUNK: usize = REPORT_BYTES - 2;

/// Compact JSON with every non-ASCII char as `\uXXXX` (Python
/// `json.dumps(separators=(",", ":"))`, `ensure_ascii=True`). Used for the
/// wire and for keymap.json bytes, so key order follows the `Value`.
pub fn to_ascii_json(value: &Value) -> String {
    // serde_json already escapes `"`, `\` and C0 controls, so every char at
    // or above DEL is inside a string literal. Python escapes DEL too.
    let compact = value.to_string();
    let mut out = String::with_capacity(compact.len());
    for c in compact.chars() {
        if (c as u32) < 0x7F {
            out.push(c);
        } else {
            for unit in c.encode_utf16(&mut [0; 2]) {
                let _ = write!(out, "\\u{unit:04x}");
            }
        }
    }
    out
}

/// Split one serialised message into output reports (each passed to
/// `IOHIDDeviceSetReport` with report id 6, type Output).
pub fn frame_message(payload: &str, framing: Framing) -> Vec<Vec<u8>> {
    let (size, prefix) = if framing.prefixed {
        (REPORT_BYTES_BLE, 1)
    } else {
        (REPORT_BYTES, 0)
    };
    let mut data = payload.as_bytes().to_vec();
    if framing.crlf {
        data.extend_from_slice(b"\r\n");
    }
    data.chunks(MAX_CHUNK)
        .map(|chunk| {
            let mut report = vec![0u8; size];
            if framing.prefixed {
                report[0] = REPORT_ID;
            }
            report[prefix] = OPCODE_DATA;
            report[prefix + 1] = chunk.len() as u8;
            report[prefix + 2..prefix + 2 + chunk.len()].copy_from_slice(chunk);
            report
        })
        .collect()
}

/// Reassemble CRLF-terminated JSON objects from input reports. Accepts a
/// buffer with or without the leading report id. Non-JSON and non-object
/// lines are dropped.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> FrameDecoder {
        FrameDecoder::default()
    }

    /// Consume one report; return the messages it completed. Never panics:
    /// it runs inside the IOKit input callback.
    pub fn feed(&mut self, raw: &[u8]) -> Vec<Value> {
        let start = if raw.len() >= 3 && raw[0] == REPORT_ID && raw[1] == OPCODE_DATA {
            1
        } else if raw.len() >= 2 && raw[0] == OPCODE_DATA {
            0
        } else {
            return Vec::new();
        };
        let length = raw[start + 1] as usize;
        let end = (start + 2 + length).min(raw.len());
        self.buffer.extend_from_slice(&raw[start + 2..end]);

        let mut messages = Vec::new();
        while let Some(pos) = self.buffer.windows(2).position(|w| w == b"\r\n") {
            let line: Vec<u8> = self.buffer.drain(..pos + 2).take(pos).collect();
            let text = String::from_utf8_lossy(&line);
            // A truncated or non-JSON frame is not fatal.
            if let Ok(parsed @ Value::Object(_)) = serde_json::from_str(text.trim()) {
                messages.push(parsed);
            }
        }
        messages
    }

    pub fn reset(&mut self) {
        self.buffer.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const USB: Framing = Framing {
        prefixed: false,
        crlf: true,
    };
    const BLE: Framing = Framing {
        prefixed: true,
        crlf: true,
    };
    const CM2: Framing = Framing {
        prefixed: true,
        crlf: false,
    };

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn to_ascii_json_escapes_like_python_ensure_ascii() {
        let cases = [
            (
                json!({"m": "x", "p": [1, true, null]}),
                r#"{"m":"x","p":[1,true,null]}"#,
            ),
            (json!({"t": "café"}), r#"{"t":"caf\u00e9"}"#),
            (json!({"t": "😀"}), r#"{"t":"\ud83d\ude00"}"#),
            (json!({"t": "a\u{7f}\n\"\\"}), r#"{"t":"a\u007f\n\"\\"}"#),
            (json!({"z": 1, "a": 2}), r#"{"z":1,"a":2}"#),
        ];
        for (value, expected) in cases {
            assert_eq!(to_ascii_json(&value), expected);
        }
    }

    #[test]
    fn frame_message_lays_out_each_framing() {
        // (framing, report size, data offset, body)
        let cases = [
            (USB, REPORT_BYTES, 0, &b"{\"m\":\"x\"}\r\n"[..]),
            (BLE, REPORT_BYTES_BLE, 1, &b"{\"m\":\"x\"}\r\n"[..]),
            (CM2, REPORT_BYTES_BLE, 1, &b"{\"m\":\"x\"}"[..]),
        ];
        for (framing, size, prefix, body) in cases {
            let reports = frame_message(r#"{"m":"x"}"#, framing);
            assert_eq!(reports.len(), 1);
            let report = &reports[0];
            assert_eq!(report.len(), size);
            if prefix == 1 {
                assert_eq!(report[0], REPORT_ID);
            }
            assert_eq!(report[prefix], OPCODE_DATA);
            assert_eq!(report[prefix + 1] as usize, body.len());
            assert_eq!(&report[prefix + 2..prefix + 2 + body.len()], body);
            assert!(report[prefix + 2 + body.len()..].iter().all(|&b| b == 0));
        }
    }

    #[test]
    fn frame_message_splits_long_payloads_into_equal_data_chunks() {
        let payload = "x".repeat(MAX_CHUNK * 2);
        let usb = frame_message(&payload, USB);
        let ble = frame_message(&payload, BLE);
        assert_eq!(usb.len(), 3); // two full chunks plus the CRLF tail
        assert!(usb.iter().all(|r| r.len() == REPORT_BYTES));
        assert_eq!(
            usb.iter().map(|r| r[1] as usize).sum::<usize>(),
            payload.len() + 2
        );
        let ble_lens: Vec<u8> = ble.iter().map(|r| r[2]).collect();
        assert_eq!(usb.iter().map(|r| r[1]).collect::<Vec<_>>(), ble_lens);
        assert_eq!(frame_message(&payload, CM2).len(), 2);
    }

    #[test]
    fn decoder_round_trips_framed_messages() {
        // The pad's replies always end in CRLF, so only CRLF framings apply.
        let message =
            json!({"m": "v.oai.hid", "p": {"k": "AG02", "act": 1, "pad": "y".repeat(200)}});
        for framing in [USB, BLE] {
            let mut decoder = FrameDecoder::new();
            let reports = frame_message(&to_ascii_json(&message), framing);
            assert!(reports.len() > 1);
            let out: Vec<Value> = reports.iter().flat_map(|r| decoder.feed(r)).collect();
            assert_eq!(out, vec![message.clone()]);
        }
    }

    #[test]
    fn decoder_accepts_a_leading_report_id() {
        let mut decoder = FrameDecoder::new();
        let mut raw = vec![REPORT_ID];
        raw.extend(&frame_message(r#"{"m":"ping"}"#, USB)[0]);
        assert_eq!(decoder.feed(&raw), vec![json!({"m": "ping"})]);
    }

    #[test]
    fn decoder_ignores_junk_and_recovers() {
        let mut decoder = FrameDecoder::new();
        assert!(decoder.feed(&[0, 0, 0]).is_empty()); // wrong opcode
        assert!(decoder.feed(&[]).is_empty());
        assert!(decoder.feed(&frame_message("not json", USB)[0]).is_empty());
        assert!(decoder.feed(&frame_message("[1,2]", USB)[0]).is_empty());
        assert!(decoder.feed(&frame_message("", USB)[0]).is_empty());
        // A length byte longer than the report is clamped, not a panic.
        assert!(decoder.feed(&[OPCODE_DATA, 200, b'{']).is_empty());
        decoder.reset();
        assert_eq!(
            decoder.feed(&frame_message(r#"{"m":"ok"}"#, USB)[0]),
            vec![json!({"m": "ok"})]
        );
    }

    #[test]
    fn decoder_reads_recorded_reports_from_keys_log() {
        // keys.log, fw 0.6.3 over USB: a single-report ack, then a
        // device.status reply spread across three reports.
        let mut decoder = FrameDecoder::new();
        let ack = hex("0602367b22726573756c74223a7b226f6b223a317d2c226964223a3935392c226d6574686f64223a22762e6f61692e726762636667227d0d0a00000000000000");
        assert_eq!(
            decoder.feed(&ack),
            vec![json!({"result": {"ok": 1}, "id": 959, "method": "v.oai.rgbcfg"})]
        );
        let parts = [
            "06023d7b22726573756c74223a7b2276657273696f6e223a22302e362e33222c2270726f66696c655f696e646578223a302c226c617965725f696e646578223a",
            "06023d332c2262617474657279223a39392c2269735f6368617267696e67223a747275657d2c226964223a3533302c226d6574686f64223a226465766963652e",
            "06020a737461747573227d0d0a000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
        ];
        assert!(decoder.feed(&hex(parts[0])).is_empty());
        assert!(decoder.feed(&hex(parts[1])).is_empty());
        assert_eq!(
            decoder.feed(&hex(parts[2])),
            vec![json!({
                "result": {"version": "0.6.3", "profile_index": 0, "layer_index": 3, "battery": 99, "is_charging": true},
                "id": 530,
                "method": "device.status"
            })]
        );
    }
}
