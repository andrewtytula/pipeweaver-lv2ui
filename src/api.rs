//! Client for the Pipeweaver daemon's websocket, the same one its web UI
//! uses. We work on the JSON directly rather than Pipeweaver's Rust types so
//! this tool keeps building when unrelated parts of the daemon's API change.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::io::ErrorKind;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Error as WsError, Message};

pub const DEFAULT_URL: &str = "ws://localhost:14565/api/websocket";

#[derive(Debug)]
pub enum ApiEvent {
    Status(Value),
    /// JSON-patch operations against the status
    Patch(Vec<Value>),
    /// The daemon rejected a command
    Rejected(String),
    Closed(String),
}

pub struct Api {
    commands: Sender<Value>,
    pub events: Receiver<ApiEvent>,
}

impl Api {
    pub fn connect(url: &str) -> Result<Api> {
        let (mut socket, _) = tungstenite::connect(url).with_context(|| {
            format!("connecting to the Pipeweaver daemon at {url} (is it running?)")
        })?;

        // A short read timeout lets one thread both read and send
        match socket.get_mut() {
            MaybeTlsStream::Plain(stream) => {
                stream.set_read_timeout(Some(Duration::from_millis(20)))?
            }
            _ => bail!("only plain ws:// connections are supported"),
        }

        let (command_tx, command_rx) = mpsc::channel::<Value>();
        let (event_tx, event_rx) = mpsc::channel();

        thread::spawn(move || {
            let mut next_id = 0u64;
            let closed = loop {
                loop {
                    match command_rx.try_recv() {
                        Ok(data) => {
                            next_id += 1;
                            let request = json!({ "id": next_id, "data": data });
                            if let Err(e) = socket.send(Message::text(request.to_string())) {
                                let _ = event_tx.send(ApiEvent::Closed(e.to_string()));
                                return;
                            }
                        }
                        Err(mpsc::TryRecvError::Empty) => break,
                        // The Api was dropped: we are done
                        Err(mpsc::TryRecvError::Disconnected) => {
                            let _ = socket.close(None);
                            return;
                        }
                    }
                }

                match socket.read() {
                    Ok(Message::Text(text)) => {
                        if let Some(event) = parse_response(text.as_str())
                            && event_tx.send(event).is_err()
                        {
                            return;
                        }
                    }
                    Ok(Message::Close(frame)) => {
                        break frame
                            .map(|f| f.reason.to_string())
                            .filter(|reason| !reason.is_empty())
                            .unwrap_or_else(|| "the daemon closed the connection".to_string());
                    }
                    Ok(_) => {}
                    Err(WsError::Io(e))
                        if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                    Err(e) => break e.to_string(),
                }
            };
            let _ = event_tx.send(ApiEvent::Closed(closed));
        });

        Ok(Api {
            commands: command_tx,
            events: event_rx,
        })
    }

    pub fn request_status(&self) {
        let _ = self.commands.send(json!("GetStatus"));
    }

    /// Fetch the daemon status, discarding anything that arrives before it.
    pub fn status(&self) -> Result<Value> {
        self.request_status();
        loop {
            match self.events.recv_timeout(Duration::from_secs(5)) {
                Ok(ApiEvent::Status(status)) => return Ok(status),
                Ok(ApiEvent::Closed(reason)) => {
                    bail!("lost the connection to Pipeweaver: {reason}")
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {
                    bail!("Pipeweaver did not answer the status request")
                }
                Err(RecvTimeoutError::Disconnected) => bail!("lost the connection to Pipeweaver"),
            }
        }
    }

    pub fn set_filter_values(&self, filter: &str, values: &[(u32, Value)]) {
        let values: Vec<Value> = values
            .iter()
            .map(|(id, value)| json!({ "id": id, "value": value }))
            .collect();
        let _ = self
            .commands
            .send(json!({ "Pipewire": { "SetFilterValues": [filter, values] } }));
    }
}

fn parse_response(text: &str) -> Option<ApiEvent> {
    let response: Value = serde_json::from_str(text).ok()?;
    let data = response.get("data")?;

    if let Some(status) = data.get("Status") {
        return Some(ApiEvent::Status(status.clone()));
    }
    if let Some(patch) = data.get("Patch").and_then(Value::as_array) {
        return Some(ApiEvent::Patch(patch.clone()));
    }
    if let Some(error) = data.get("Err").and_then(Value::as_str) {
        return Some(ApiEvent::Rejected(error.to_string()));
    }
    if let Some(error) = data.pointer("/Pipewire/Err").and_then(Value::as_str) {
        return Some(ApiEvent::Rejected(error.to_string()));
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ValueKind {
    Bool,
    Int,
    Float,
}

impl ValueKind {
    /// Wrap a raw control value the way the daemon expects for this parameter.
    pub fn encode(self, value: f32) -> Value {
        match self {
            ValueKind::Bool => json!({ "Bool": value > 0.5 }),
            ValueKind::Int => json!({ "Int32": value.round() as i32 }),
            ValueKind::Float => json!({ "Float32": value }),
        }
    }
}

/// Read a daemon `FilterValue` (`{"Float32": 1.5}`) as a raw control value.
pub fn decode_value(value: &Value) -> Option<(ValueKind, f32)> {
    let (kind, inner) = value.as_object()?.iter().next()?;
    match kind.as_str() {
        "Bool" => Some((ValueKind::Bool, if inner.as_bool()? { 1.0 } else { 0.0 })),
        "Int32" => Some((ValueKind::Int, inner.as_i64()? as f32)),
        "Float32" => Some((ValueKind::Float, inner.as_f64()? as f32)),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct Param {
    /// Position in the daemon's parameter array, which patch paths refer to
    pub position: usize,
    /// The id `SetFilterValues` wants
    pub id: u32,
    pub symbol: String,
    pub kind: ValueKind,
    pub value: f32,
    pub is_input: bool,
}

#[derive(Debug, Clone)]
pub struct FilterEntry {
    pub id: String,
    pub device: String,
    pub plugin_uri: String,
    pub name: String,
    pub running: bool,
    pub state: String,
    /// Position among filters with the same plugin on the same device
    pub ordinal: usize,
}

/// Each device's name and filter chain, sources first.
fn device_chains(status: &Value) -> Vec<(&str, &[Value])> {
    let mut chains = Vec::new();
    for direction in ["sources", "targets"] {
        for class in ["physical_devices", "virtual_devices"] {
            let path = format!("/audio/profile/devices/{direction}/{class}");
            let devices = status.pointer(&path).and_then(Value::as_array);

            for device in devices.into_iter().flatten() {
                let name = device
                    .pointer("/description/name")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                let filters = device
                    .get("filters")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                chains.push((name, filters));
            }
        }
    }
    chains
}

/// Every LV2 filter in the profile, in device then chain order.
pub fn lv2_filters(status: &Value) -> Vec<FilterEntry> {
    let mut entries = Vec::new();
    for (device_name, filters) in device_chains(status) {
        let mut seen: Vec<&str> = Vec::new();
        for filter in filters {
            let (Some(id), Some(uri)) = (
                filter.get("id").and_then(Value::as_str),
                filter
                    .pointer("/filter/LV2/plugin_uri")
                    .and_then(Value::as_str),
            ) else {
                continue;
            };

            let config = status.pointer(&format!("/audio/filter_config/{id}"));
            let state = config.and_then(|c| c.get("state"));
            let running = state.and_then(Value::as_str) == Some("Running");
            entries.push(FilterEntry {
                id: id.to_string(),
                device: device_name.to_string(),
                plugin_uri: uri.to_string(),
                name: config
                    .and_then(|c| c.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or(uri)
                    .to_string(),
                running,
                state: match state {
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => other.to_string(),
                    None => "Unknown".to_string(),
                },
                ordinal: seen.iter().filter(|u| **u == uri).count(),
            });
            seen.push(uri);
        }
    }
    entries
}

/// The filters that are Pipeweaver's own rather than LV2 plugins, as "device / name".
pub fn builtin_filters(status: &Value) -> Vec<String> {
    let mut names = Vec::new();
    for (device_name, filters) in device_chains(status) {
        for filter in filters {
            let Some(id) = filter.get("id").and_then(Value::as_str) else {
                continue;
            };
            if filter.pointer("/filter/LV2").is_some() {
                continue;
            }
            let name = status
                .pointer(&format!("/audio/filter_config/{id}/name"))
                .and_then(Value::as_str)
                .or_else(|| filter.pointer("/filter/PWV/plugin_type")?.as_str())
                .unwrap_or(id);
            names.push(format!("{device_name} / {name}"));
        }
    }
    names
}

pub fn filter_params(status: &Value, filter: &str) -> Result<Vec<Param>> {
    let parameters = status
        .pointer(&format!("/audio/filter_config/{filter}/parameters"))
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("Pipeweaver has no running filter {filter}"))?;

    Ok(parameters
        .iter()
        .enumerate()
        .filter_map(|(position, p)| {
            let (kind, value) = decode_value(p.get("value")?)?;
            Some(Param {
                position,
                id: p.get("id")?.as_u64()? as u32,
                symbol: p.get("symbol")?.as_str()?.to_string(),
                kind,
                value,
                is_input: p.get("is_input").and_then(Value::as_bool).unwrap_or(true),
            })
        })
        .collect())
}

/// What a single patch operation means for the filter we are showing.
#[derive(Debug, PartialEq)]
pub enum PatchEffect {
    /// Parameter at this array position now has this value
    Value(usize, f32),
    /// Something structural changed (filter recreated, removed, state change): re-read the status
    Resync,
    Unrelated,
}

pub fn patch_effect(operation: &Value, filter: &str) -> PatchEffect {
    let Some(path) = operation.get("path").and_then(Value::as_str) else {
        return PatchEffect::Unrelated;
    };
    let prefix = format!("/audio/filter_config/{filter}");

    let Some(rest) = path.strip_prefix(&prefix) else {
        // A parent of our filter was replaced wholesale
        return if prefix.starts_with(path) {
            PatchEffect::Resync
        } else {
            PatchEffect::Unrelated
        };
    };
    if !rest.is_empty() && !rest.starts_with('/') {
        // A different filter whose id merely starts with ours
        return PatchEffect::Unrelated;
    }

    let mut parts = rest.split('/').skip(1);
    if parts.next() != Some("parameters") {
        return PatchEffect::Resync;
    }
    let position = parts.next().and_then(|p| p.parse::<usize>().ok());
    let position = match (position, parts.next()) {
        (Some(position), Some("value")) => position,
        // The list was replaced, or an entry was added or removed, which
        // moves every parameter after it
        (_, None) => return PatchEffect::Resync,
        // Names, ranges and the like changing does not concern us
        _ => return PatchEffect::Unrelated,
    };

    let Some(value) = operation.get("value") else {
        return PatchEffect::Resync;
    };
    // Either the whole `{"Float32": x}` was replaced, or just the number inside it
    let raw = match parts.next() {
        None => decode_value(value).map(|(_, v)| v),
        Some(_) => match value {
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            other => other.as_f64().map(|v| v as f32),
        },
    };
    match raw {
        Some(value) => PatchEffect::Value(position, value),
        None => PatchEffect::Resync,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "01M3ZB9YNC3KTK2BTAWWD8Q1S4";

    fn status() -> Value {
        json!({ "audio": {
            "profile": { "devices": {
                "sources": {
                    "physical_devices": [{
                        "description": { "name": "Mic" },
                        "filters": [
                            { "id": "A", "filter": { "PWV": { "plugin_type": "RNNoise", "values": {} } } },
                            { "id": ID, "filter": { "LV2": { "plugin_uri": "http://lsp-plug.in/plugins/lv2/gate_stereo", "values": {} } } },
                            { "id": "B", "filter": { "LV2": { "plugin_uri": "http://lsp-plug.in/plugins/lv2/gate_stereo", "values": {} } } }
                        ]
                    }],
                    "virtual_devices": []
                },
                "targets": { "physical_devices": [], "virtual_devices": [] }
            } },
            "filter_config": {
                ID: { "name": "LSP Gate Stereo", "state": "Running", "parameters": [
                    { "id": 0, "symbol": "enabled", "value": { "Bool": true }, "is_input": true },
                    { "id": 1, "symbol": "gt", "value": { "Float32": 0.25 }, "is_input": true },
                    { "id": 2, "symbol": "mode", "value": { "Int32": 3 }, "is_input": true }
                ] },
                "B": { "name": "LSP Gate Stereo", "state": { "Error": "boom" }, "parameters": [] }
            }
        } })
    }

    #[test]
    fn lists_only_lv2_filters_with_their_device_and_state() {
        let filters = lv2_filters(&status());
        assert_eq!(filters.len(), 2);
        assert_eq!(
            (filters[0].id.as_str(), filters[0].device.as_str()),
            (ID, "Mic")
        );
        assert!(filters[0].running);
        assert_eq!(filters[0].ordinal, 0);
        assert!(!filters[1].running);
        assert_eq!(filters[1].state, r#"{"Error":"boom"}"#);
        assert_eq!(filters[1].ordinal, 1);
    }

    #[test]
    fn names_the_filters_that_are_not_lv2() {
        assert_eq!(builtin_filters(&status()), ["Mic / RNNoise"]);
    }

    #[test]
    fn reads_parameters_with_their_kinds() {
        let params = filter_params(&status(), ID).unwrap();
        let summary: Vec<_> = params
            .iter()
            .map(|p| (p.symbol.as_str(), p.kind, p.value))
            .collect();
        assert_eq!(
            summary,
            [
                ("enabled", ValueKind::Bool, 1.0),
                ("gt", ValueKind::Float, 0.25),
                ("mode", ValueKind::Int, 3.0)
            ]
        );
        assert!(filter_params(&status(), "missing").is_err());
    }

    #[test]
    fn values_encode_in_the_kind_the_daemon_expects() {
        assert_eq!(ValueKind::Bool.encode(1.0), json!({ "Bool": true }));
        assert_eq!(ValueKind::Int.encode(2.6), json!({ "Int32": 3 }));
        assert_eq!(ValueKind::Float.encode(0.5), json!({ "Float32": 0.5 }));
    }

    #[test]
    fn patches_are_classified() {
        let op =
            |path: &str, value: Value| json!({ "op": "replace", "path": path, "value": value });
        let base = format!("/audio/filter_config/{ID}");

        assert_eq!(
            patch_effect(
                &op(&format!("{base}/parameters/1/value/Float32"), json!(0.5)),
                ID
            ),
            PatchEffect::Value(1, 0.5)
        );
        assert_eq!(
            patch_effect(
                &op(&format!("{base}/parameters/0/value/Bool"), json!(false)),
                ID
            ),
            PatchEffect::Value(0, 0.0)
        );
        assert_eq!(
            patch_effect(
                &op(&format!("{base}/parameters/2/value"), json!({ "Int32": 4 })),
                ID
            ),
            PatchEffect::Value(2, 4.0)
        );
        assert_eq!(
            patch_effect(&op(&format!("{base}/state"), json!("Stopped")), ID),
            PatchEffect::Resync
        );
        assert_eq!(
            patch_effect(&json!({ "op": "remove", "path": base }), ID),
            PatchEffect::Resync
        );
        assert_eq!(
            patch_effect(
                &json!({ "op": "remove", "path": format!("{base}/parameters/1") }),
                ID
            ),
            PatchEffect::Resync
        );
        assert_eq!(
            patch_effect(&op(&format!("{base}/parameters"), json!([])), ID),
            PatchEffect::Resync
        );
        assert_eq!(
            patch_effect(&op(&format!("{base}/parameters/1/name"), json!("x")), ID),
            PatchEffect::Unrelated
        );
        assert_eq!(
            patch_effect(&op("/audio/filter_config", json!({})), ID),
            PatchEffect::Resync
        );
        assert_eq!(
            patch_effect(
                &op(
                    "/audio/filter_config/OTHER/parameters/1/value/Float32",
                    json!(1.0)
                ),
                ID
            ),
            PatchEffect::Unrelated
        );
        assert_eq!(
            patch_effect(
                &op(&format!("/audio/profile/devices/x/{ID}"), json!(1.0)),
                ID
            ),
            PatchEffect::Unrelated
        );
    }
}
