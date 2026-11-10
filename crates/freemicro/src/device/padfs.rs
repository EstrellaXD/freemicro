//! Sequential, id-matched access to the pad's own files (`keymap.json`,
//! `smart_actions.json`). Port of `device/padfs.py`.
//!
//! Writing can brick a pad if another host talks during it, so: one request
//! at a time, long envelope, wait for the reply with *our* id, 50 ms gap,
//! 3072-byte chunks, never abort halfway, verify by SHA-1 from
//! `fs.list {checksum: true}`. The caller guarantees it is the only host.

use std::fmt;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};

use super::{request, PadIo, Request, MAX_REQUEST_ID};

/// Raw bytes per chunk (4096 base64 chars).
pub const CHUNK: usize = 3072;
pub const GAP: Duration = Duration::from_millis(50);
pub const TIMEOUT: Duration = Duration::from_secs(5);
pub const KEYMAP: &str = "keymap.json";
pub const SMART_ACTIONS: &str = "smart_actions.json";
/// Ids cycle through 500..999, as Input's do.
pub const FIRST_ID: u32 = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PadFsError(pub String);

impl fmt::Display for PadFsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PadFsError {}

pub struct PadFs<'a> {
    pub io: &'a mut dyn PadIo,
    next_id: u32,
    /// [`GAP`] in production; zero in tests.
    pub gap: Duration,
    /// [`TIMEOUT`]; short in tests.
    timeout: Duration,
}

impl<'a> PadFs<'a> {
    pub fn new(io: &'a mut dyn PadIo) -> PadFs<'a> {
        PadFs {
            io,
            next_id: FIRST_ID,
            gap: GAP,
            timeout: TIMEOUT,
        }
    }

    /// One request; its `result`, or an error for timeout / `error` reply.
    /// Stray messages (key events) during a file operation are dropped.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, PadFsError> {
        let id = self.next_id;
        self.next_id = if id + 1 >= MAX_REQUEST_ID {
            FIRST_ID
        } else {
            id + 1
        };
        let req = Request {
            method,
            params: Some(params),
            id,
            timeout: self.timeout,
            match_id: true,
            standard: true,
        };
        let reply = request(self.io, &req, &mut |_| {})
            .map_err(|e| PadFsError(format!("{method}: {e}")))?;
        if !self.gap.is_zero() {
            std::thread::sleep(self.gap);
        }
        let Some(mut reply) = reply else {
            return Err(PadFsError(format!("{method}: the pad did not answer")));
        };
        if let Some(error) = reply.get("error") {
            return Err(PadFsError(format!("{method}: {error}")));
        }
        Ok(reply
            .get_mut("result")
            .map(Value::take)
            .unwrap_or(Value::Null))
    }

    /// `fs.readbin` in CHUNK steps until `total_size`.
    pub fn read(&mut self, name: &str) -> Result<Vec<u8>, PadFsError> {
        let mut data = Vec::new();
        loop {
            let result = self.call(
                "fs.readbin",
                json!({"file": name, "offset": data.len(), "len": CHUNK}),
            )?;
            let unexpected = || PadFsError(format!("fs.readbin {name}: unexpected reply {result}"));
            let encoded = result
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(unexpected)?;
            let chunk = B64.decode(encoded).map_err(|_| unexpected())?;
            data.extend_from_slice(&chunk);
            let total = result
                .get("total_size")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if data.len() as u64 >= total || chunk.is_empty() {
                return Ok(data);
            }
        }
    }

    /// `fs.writebin {file, data, append: true, offset, completed}`.
    pub fn write(&mut self, name: &str, data: &[u8]) -> Result<(), PadFsError> {
        // A 0-byte file is still one (empty, completed) chunk.
        let last = (data.len().max(1) - 1) / CHUNK * CHUNK;
        for offset in (0..=last).step_by(CHUNK) {
            let chunk = &data[offset..(offset + CHUNK).min(data.len())];
            self.call(
                "fs.writebin",
                json!({
                    "file": name,
                    "data": B64.encode(chunk),
                    "append": true,
                    "offset": offset,
                    "completed": offset == last,
                }),
            )?;
        }
        Ok(())
    }

    /// The pad's SHA-1 hex of `name`, or `None` if absent.
    pub fn checksum(&mut self, name: &str) -> Result<Option<String>, PadFsError> {
        let entries = self.call("fs.list", json!({"checksum": true}))?;
        Ok(entries
            .as_array()
            .into_iter()
            .flatten()
            .find(|entry| entry.get("name").and_then(Value::as_str) == Some(name))
            .and_then(|entry| entry.get("checksum").and_then(Value::as_str))
            .map(str::to_string))
    }

    /// Write, then prove the pad holds exactly `data`.
    pub fn write_verified(&mut self, name: &str, data: &[u8]) -> Result<(), PadFsError> {
        self.write(name, data)?;
        let expected: String = Sha1::digest(data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let actual = self.checksum(name)?;
        if actual.as_deref().map(str::to_lowercase).as_deref() != Some(expected.as_str()) {
            return Err(PadFsError(format!(
                "{name}: checksum after write is {}, expected {expected}",
                actual.as_deref().unwrap_or("None")
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::rc::Rc;

    use super::*;
    use crate::device::fake::FakePad;

    type Files = Rc<RefCell<BTreeMap<String, Vec<u8>>>>;

    /// Answers fs.* like firmware 0.6.3: reads `params` only from the long
    /// envelope (a short `p` gets "Missing data parameter"), echoes the id,
    /// and puts a key event before each reply.
    fn pad(files: &Files, corrupt: bool) -> FakePad {
        let files = files.clone();
        FakePad::creator_micro_2().respond(move |msg| {
            let id = msg["id"].clone();
            let Some(params) = msg.get("params") else {
                return vec![
                    json!({"error": {"code": 400, "message": "Missing data parameter"}, "id": id}),
                ];
            };
            let file = params["file"].as_str().unwrap_or_default().to_string();
            let mut files = files.borrow_mut();
            let result = match msg["method"].as_str().unwrap() {
                "fs.readbin" => {
                    let data = &files[&file];
                    let offset = params["offset"].as_u64().unwrap() as usize;
                    let end = (offset + params["len"].as_u64().unwrap() as usize).min(data.len());
                    json!({"total_size": data.len(), "data": B64.encode(&data[offset..end])})
                }
                "fs.writebin" => {
                    let body = B64.decode(params["data"].as_str().unwrap()).unwrap();
                    let entry = files.entry(file).or_default();
                    if params["offset"] == 0 {
                        entry.clear();
                    }
                    entry.extend(body);
                    json!({"data_written": params["data"].as_str().unwrap().len()})
                }
                _ => files
                    .iter()
                    .map(|(name, data)| {
                        let mut hashed = data.clone();
                        if corrupt {
                            hashed.push(b'x');
                        }
                        let sum: String = Sha1::digest(&hashed)
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect();
                        json!({"name": name, "size": data.len(), "checksum": sum})
                    })
                    .collect(),
            };
            vec![
                json!({"m": "v.oai.hid", "p": {"k": "AG00", "act": 1}}),
                json!({"result": result, "id": id, "method": msg["method"]}),
            ]
        })
    }

    fn writes(pad: &FakePad) -> Vec<(u64, bool)> {
        pad.sent
            .iter()
            .filter(|m| m["method"] == "fs.writebin")
            .map(|m| {
                let p = &m["params"];
                assert_eq!(p["append"], true);
                (p["offset"].as_u64().unwrap(), p["completed"] == true)
            })
            .collect()
    }

    #[test]
    fn write_verified_chunks_in_order_and_round_trips() {
        let cases = [
            (
                (0..=255u8).cycle().take(7680).collect::<Vec<u8>>(), // three chunks
                vec![(0, false), (3072, false), (6144, true)],
            ),
            (vec![b'{'; CHUNK], vec![(0, true)]),
            (Vec::new(), vec![(0, true)]), // one empty chunk
        ];
        for (data, expected) in cases {
            let files = Files::default();
            let mut io = pad(&files, false);
            let mut fs = PadFs::new(&mut io);
            fs.gap = Duration::ZERO;
            fs.write_verified(KEYMAP, &data).unwrap();
            assert_eq!(fs.read(KEYMAP).unwrap(), data);
            assert_eq!(writes(&io), expected);
            assert!(io.sent.iter().all(|m| m.get("p").is_none()));
        }
    }

    #[test]
    fn write_verified_fails_on_checksum_mismatch_or_missing_file() {
        let files = Files::default();
        let mut io = pad(&files, true);
        let mut fs = PadFs::new(&mut io);
        fs.gap = Duration::ZERO;
        assert!(fs.write_verified(KEYMAP, b"{}").is_err());
        assert_eq!(fs.checksum(SMART_ACTIONS).unwrap(), None);
    }

    #[test]
    fn call_reports_an_error_reply_or_silence() {
        let refuses = FakePad::creator_micro_2().respond(|msg| {
            vec![json!({"error": {"code": 404, "message": "no file"}, "id": msg["id"]})]
        });
        let cases = [
            (refuses, r#"fs.readbin: {"code":404,"message":"no file"}"#),
            (
                FakePad::creator_micro_2(),
                "fs.readbin: the pad did not answer",
            ),
        ];
        for (mut io, expected) in cases {
            let mut fs = PadFs::new(&mut io);
            fs.gap = Duration::ZERO;
            fs.timeout = Duration::from_millis(10);
            assert_eq!(fs.read(KEYMAP), Err(PadFsError(expected.into())));
        }
    }

    #[test]
    fn call_ids_cycle_below_999() {
        let mut io =
            FakePad::creator_micro_2().respond(|msg| vec![json!({"result": 1, "id": msg["id"]})]);
        let mut fs = PadFs::new(&mut io);
        fs.gap = Duration::ZERO;
        for _ in 0..(MAX_REQUEST_ID - FIRST_ID + 1) {
            fs.call("fs.list", json!({})).unwrap();
        }
        let ids: Vec<u64> = io.sent.iter().map(|m| m["id"].as_u64().unwrap()).collect();
        assert_eq!(ids[0], 500);
        assert_eq!(ids[498], 998);
        assert_eq!(ids[499], 500);
    }
}
