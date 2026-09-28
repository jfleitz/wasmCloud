//! SPI bus plugin for WebAssembly components.
//!
//! This plugin implements the `wasi:spi/spi@0.2.0-draft` interface, giving
//! components half- and full-duplex access to SPI peripherals attached to the
//! host — for example APA102/SK9822 addressable LED strips driven from a
//! Raspberry Pi in a pinball machine.
//!
//! # Backends
//!
//! - [`PhysicalSpi`] (Linux only) drives real hardware through the kernel
//!   `spidev` interface (`/dev/spidevB.C`).
//! - [`VirtualSpi`] records written frames in memory on any platform, for
//!   tests and virtual-hardware simulation.
//!
//! Components open *logical* device names (e.g. `"playfield-leds"`); the
//! mapping to a concrete `/dev/spidev*` path, clock speed, and SPI mode is
//! host configuration. Devices are declared when constructing the plugin and
//! can be added or overridden per workload through `wasi:spi` interface
//! config using dotted keys:
//!
//! ```text
//! playfield-leds.path = /dev/spidev0.0
//! playfield-leds.speed-hz = 8000000
//! playfield-leds.mode = 0
//! ```
//!
//! # Driving APA102 LEDs
//!
//! APA102 strips are write-only SPI mode 0 devices: the component builds the
//! frame (4-byte zero start frame, then `0xE0 | brightness, B, G, R` per LED,
//! then an end frame of at least n/2 clock pulses) and sends it with a single
//! `write` call. The kernel's default spidev transfer buffer is 4096 bytes,
//! which comfortably fits several hundred LEDs; raise the `spidev.bufsiz`
//! kernel module parameter and [`PhysicalSpi`]'s `max_transfer` together for
//! longer strips.

use std::collections::HashMap;
use std::path::PathBuf;

#[cfg(target_os = "linux")]
mod physical;
mod virt;

#[cfg(target_os = "linux")]
pub use physical::PhysicalSpi;
pub use virt::VirtualSpi;

const WASI_SPI_ID: &str = "wasi-spi";
const WASI_SPI_INTERFACE: &str = "wasi:spi/spi@0.2.0-draft";

/// Host-side settings for one logical SPI device.
///
/// Defaults suit APA102 LED strips: SPI mode 0, 8 bits per word, MSB first,
/// 4 MHz clock (conservative enough for a few meters of wiring; APA102 parts
/// tolerate much faster clocks over short runs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpiDeviceConfig {
    /// Device node, e.g. `/dev/spidev0.0` (bus 0, chip-select 0).
    pub path: PathBuf,
    /// Maximum clock speed in Hz.
    pub speed_hz: u32,
    /// SPI mode (0–3): clock polarity and phase.
    pub mode: u8,
    /// Word size in bits.
    pub bits_per_word: u8,
    /// Clock out least-significant bit first.
    pub lsb_first: bool,
}

impl SpiDeviceConfig {
    /// Creates a config for `path` with APA102-friendly defaults.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            speed_hz: 4_000_000,
            mode: 0,
            bits_per_word: 8,
            lsb_first: false,
        }
    }
}

/// Parses per-workload device definitions out of `wasi:spi` interface config.
///
/// Keys are `<device-name>.<field>` where field is one of `path`,
/// `speed-hz`, `mode`, `bits-per-word`, or `lsb-first`. Every named device
/// must define `path`. Keys without a recognized field suffix are ignored so
/// unrelated config can share the namespace.
fn device_configs_from_interface(
    config: &HashMap<String, String>,
) -> anyhow::Result<HashMap<String, SpiDeviceConfig>> {
    // Collect fields per device name first so declaration order doesn't matter.
    let mut fields: HashMap<&str, HashMap<&str, &str>> = HashMap::new();
    for (key, value) in config {
        let Some((name, field)) = key.rsplit_once('.') else {
            continue;
        };
        match field {
            "path" | "speed-hz" | "mode" | "bits-per-word" | "lsb-first" => {
                fields.entry(name).or_default().insert(field, value);
            }
            _ => {
                tracing::debug!(key, "ignoring unrecognized wasi:spi config key");
            }
        }
    }

    let mut devices = HashMap::new();
    for (name, fields) in fields {
        let Some(path) = fields.get("path") else {
            anyhow::bail!("wasi:spi device '{name}' is missing required config key '{name}.path'");
        };
        let mut device = SpiDeviceConfig::new(*path);
        if let Some(speed) = fields.get("speed-hz") {
            device.speed_hz = speed
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid '{name}.speed-hz' value '{speed}': {e}"))?;
        }
        if let Some(mode) = fields.get("mode") {
            device.mode = mode
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid '{name}.mode' value '{mode}': {e}"))?;
            anyhow::ensure!(
                device.mode <= 3,
                "invalid '{name}.mode' value '{mode}': SPI mode must be 0-3"
            );
        }
        if let Some(bits) = fields.get("bits-per-word") {
            device.bits_per_word = bits.parse().map_err(|e| {
                anyhow::anyhow!("invalid '{name}.bits-per-word' value '{bits}': {e}")
            })?;
        }
        if let Some(lsb) = fields.get("lsb-first") {
            device.lsb_first = lsb
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid '{name}.lsb-first' value '{lsb}': {e}"))?;
        }
        devices.insert(name.to_string(), device);
    }
    Ok(devices)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_device_with_defaults() {
        let config = HashMap::from([("leds.path".to_string(), "/dev/spidev0.0".to_string())]);
        let devices = device_configs_from_interface(&config).unwrap_or_default();
        assert_eq!(
            devices.get("leds"),
            Some(&SpiDeviceConfig::new("/dev/spidev0.0"))
        );
    }

    #[test]
    fn parses_all_fields() {
        let config = HashMap::from([
            ("leds.path".to_string(), "/dev/spidev0.1".to_string()),
            ("leds.speed-hz".to_string(), "8000000".to_string()),
            ("leds.mode".to_string(), "3".to_string()),
            ("leds.bits-per-word".to_string(), "16".to_string()),
            ("leds.lsb-first".to_string(), "true".to_string()),
        ]);
        let devices = device_configs_from_interface(&config).unwrap_or_default();
        assert_eq!(
            devices.get("leds"),
            Some(&SpiDeviceConfig {
                path: PathBuf::from("/dev/spidev0.1"),
                speed_hz: 8_000_000,
                mode: 3,
                bits_per_word: 16,
                lsb_first: true,
            })
        );
    }

    #[test]
    fn ignores_unrelated_keys_and_parses_multiple_devices() {
        let config = HashMap::from([
            ("leds.path".to_string(), "/dev/spidev0.0".to_string()),
            ("matrix.path".to_string(), "/dev/spidev0.1".to_string()),
            ("unrelated".to_string(), "value".to_string()),
            ("also.unrelated".to_string(), "value".to_string()),
        ]);
        let devices = device_configs_from_interface(&config).unwrap_or_default();
        assert_eq!(devices.len(), 2);
        assert!(devices.contains_key("leds"));
        assert!(devices.contains_key("matrix"));
    }

    #[test]
    fn rejects_device_without_path() {
        let config = HashMap::from([("leds.speed-hz".to_string(), "8000000".to_string())]);
        assert!(device_configs_from_interface(&config).is_err());
    }

    #[test]
    fn rejects_invalid_mode() {
        let config = HashMap::from([
            ("leds.path".to_string(), "/dev/spidev0.0".to_string()),
            ("leds.mode".to_string(), "4".to_string()),
        ]);
        assert!(device_configs_from_interface(&config).is_err());
    }
}
