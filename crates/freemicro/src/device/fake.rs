//! An in-memory [`PadIo`] for tests in every module. Always compiled (no
//! IOKit), so any module's `#[cfg(test)]` code can use it.

use std::collections::VecDeque;
use std::time::Duration;

use serde_json::Value;

use super::{DeviceError, PadInfo, PadIo, Transport};

type Responder = Box<dyn FnMut(&Value) -> Vec<Value>>;

/// Records every sent message; `poll` drains `inbox`. A `responder` turns
/// each sent message into replies queued on `inbox` (e.g. recorded fixtures
/// from the scratchpad probe logs).
pub struct FakePad {
    pub info: PadInfo,
    pub sent: Vec<Value>,
    pub inbox: VecDeque<Value>,
    pub responder: Option<Responder>,
    /// When set, the next `send` or `poll` fails with this error (a drop).
    pub fail: Option<DeviceError>,
}

impl FakePad {
    /// A Creator Micro 2 (PID 0x8298) on USB.
    pub fn creator_micro_2() -> FakePad {
        FakePad::with_info(PadInfo {
            transport: Transport::Usb,
            product_id: 0x8298,
        })
    }

    pub fn with_info(info: PadInfo) -> FakePad {
        FakePad {
            info,
            sent: Vec::new(),
            inbox: VecDeque::new(),
            responder: None,
            fail: None,
        }
    }

    pub fn respond(mut self, responder: impl FnMut(&Value) -> Vec<Value> + 'static) -> FakePad {
        self.responder = Some(Box::new(responder));
        self
    }
}

impl PadIo for FakePad {
    fn info(&self) -> &PadInfo {
        &self.info
    }

    fn send(&mut self, message: &Value) -> Result<(), DeviceError> {
        if let Some(err) = self.fail.take() {
            return Err(err);
        }
        self.sent.push(message.clone());
        if let Some(responder) = self.responder.as_mut() {
            self.inbox.extend(responder(message));
        }
        Ok(())
    }

    fn poll(&mut self, _timeout: Duration) -> Result<Vec<Value>, DeviceError> {
        if let Some(err) = self.fail.take() {
            return Err(err);
        }
        Ok(self.inbox.drain(..).collect())
    }
}
