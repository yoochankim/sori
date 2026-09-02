//! Device discovery + the "which microphone should we actually use" rule.
//!
//! Rule: follow the system default input, but never pick a virtual / loopback /
//! aggregate device automatically. Those produce silence in practice (BlackHole,
//! Jump Desktop, our own tap, `Multi-Input (…+BlackHole)`), and that exact
//! failure cost us an evening on 2026-09-01.

use cidre::core_audio as ca;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct InputDevice {
    pub name: String,
    pub is_default: bool,
    /// True for devices we refuse to pick automatically.
    pub is_virtual: bool,
}

const VIRTUAL_NAME_HINTS: &[&str] = &[
    "BlackHole",
    "Jump Desktop",
    "Multi-Input",
    "Multi-Output",
    "Aggregate",
    "Loopback",
    "Soundflower",
    "VB-Cable",
];

pub fn looks_virtual(name: &str) -> bool {
    let lowercase = name.to_lowercase();
    VIRTUAL_NAME_HINTS
        .iter()
        .any(|hint| lowercase.contains(&hint.to_lowercase()))
}

pub fn list_inputs() -> Vec<InputDevice> {
    let default = ca::System::default_input_device().ok();
    ca::System::devices()
        .unwrap_or_default()
        .into_iter()
        .filter(has_input_channels)
        .filter_map(|device| {
            let name = device.name().ok()?.to_string();
            let virtual_transport = device.transport_type().is_ok_and(|transport| {
                transport == ca::DeviceTransportType::VIRTUAL
                    || transport == ca::DeviceTransportType::AGGREGATE
            });
            Some(InputDevice {
                is_default: default.as_ref() == Some(&device),
                is_virtual: virtual_transport || looks_virtual(&name),
                name,
            })
        })
        .collect()
}

/// The microphone we'd use right now in automatic mode, if any.
pub fn automatic_mic() -> Option<InputDevice> {
    let list = list_inputs();
    list.iter()
        .find(|d| d.is_default && !d.is_virtual)
        .or_else(|| list.iter().find(|d| !d.is_virtual))
        .cloned()
}

/// Resolve the effective mic. An explicit name never silently falls back.
pub fn resolve_mic(override_name: Option<&str>) -> Option<InputDevice> {
    if let Some(name) = override_name {
        return list_inputs().into_iter().find(|device| device.name == name);
    }
    automatic_mic()
}

pub fn default_output_name() -> String {
    ca::System::default_output_device()
        .ok()
        .and_then(|device| device.name().ok())
        .map(|name| name.to_string())
        .unwrap_or_else(|| "Unknown output".to_string())
}

fn has_input_channels(device: &ca::Device) -> bool {
    device.input_stream_cfg().ok().is_some_and(|config| {
        config
            .buffers()
            .iter()
            .take(config.number_buffers())
            .any(|buffer| buffer.number_channels > 0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_device_names_are_rejected() {
        assert!(looks_virtual("BlackHole 2ch"));
        assert!(looks_virtual(
            "Multi-Input (MacBook Microphone + BlackHole)"
        ));
        assert!(!looks_virtual("MacBook Pro Microphone"));
    }

    #[test]
    fn missing_explicit_device_does_not_fall_back() {
        assert!(resolve_mic(Some("definitely-not-a-real-device")).is_none());
    }
}
