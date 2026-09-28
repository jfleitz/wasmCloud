//! Minimal FTDI FIFO transport over libusb (via `rusb`).
//!
//! The P3-ROC's FT245RL/FT240X presents a plain byte FIFO: no baud/line
//! configuration matters, only the vendor requests libftdi issues at open
//! (reset, purge, latency timer) and the quirk that every inbound
//! `wMaxPacketSize` packet starts with two modem-status bytes that must be
//! stripped. Doing this directly over libusb keeps the plugin free of the
//! `ftdi_sio`-vs-D2XX driver dance — though the kernel's `ftdi_sio` module is
//! still detached from the interface automatically if it has claimed it.

use std::time::Duration;

use rusb::{Device, DeviceHandle, Direction, GlobalContext, TransferType};

use super::proto::{FTDI_FT240X_PRODUCT_ID, FTDI_FT245RL_PRODUCT_ID, FTDI_VENDOR_ID};

// FTDI vendor requests (bmRequestType 0x40).
const REQUEST_RESET: u8 = 0x00;
const REQUEST_SET_LATENCY_TIMER: u8 = 0x09;
const RESET_SIO: u16 = 0;
const RESET_PURGE_RX: u16 = 1;
const RESET_PURGE_TX: u16 = 2;

/// Matches libpinproc's `ftdi_set_latency_timer(&ftdic, 2)` — small enough
/// that switch events reach the host within a couple of milliseconds.
const LATENCY_TIMER_MS: u8 = 2;

pub struct FtdiTransport {
    handle: DeviceHandle<GlobalContext>,
    read_ep: u8,
    write_ep: u8,
    max_packet_size: usize,
}

impl FtdiTransport {
    /// Opens the first FTDI device matching the P-ROC family IDs (optionally
    /// filtered by `serial`), detaches `ftdi_sio` if needed, and initializes
    /// the FIFO (reset, latency timer, purge).
    pub fn open(serial: Option<&str>) -> anyhow::Result<Self> {
        let device = find_device(serial)?;
        let config = device.config_descriptor(0)?;
        let interface = config
            .interfaces()
            .next()
            .ok_or_else(|| anyhow::anyhow!("FTDI device has no USB interface"))?;
        let descriptor = interface
            .descriptors()
            .next()
            .ok_or_else(|| anyhow::anyhow!("FTDI interface has no descriptor"))?;

        let mut read_ep = None;
        let mut write_ep = None;
        let mut max_packet_size = 64usize;
        for endpoint in descriptor.endpoint_descriptors() {
            if endpoint.transfer_type() != TransferType::Bulk {
                continue;
            }
            match endpoint.direction() {
                Direction::In => {
                    read_ep = Some(endpoint.address());
                    max_packet_size = endpoint.max_packet_size() as usize;
                }
                Direction::Out => write_ep = Some(endpoint.address()),
            }
        }
        let (read_ep, write_ep) = read_ep.zip(write_ep).ok_or_else(|| {
            anyhow::anyhow!("FTDI device is missing its bulk IN/OUT endpoint pair")
        })?;

        let handle = device.open()?;
        // The kernel's ftdi_sio serial driver claims the interface on plug-in;
        // take it back for the FIFO protocol.
        let _ = handle.set_auto_detach_kernel_driver(true);
        handle.claim_interface(descriptor.interface_number())?;

        let transport = Self {
            handle,
            read_ep,
            write_ep,
            max_packet_size,
        };
        transport.vendor_request(REQUEST_RESET, RESET_SIO)?;
        transport.vendor_request(REQUEST_SET_LATENCY_TIMER, u16::from(LATENCY_TIMER_MS))?;
        transport.purge()?;
        Ok(transport)
    }

    fn vendor_request(&self, request: u8, value: u16) -> anyhow::Result<()> {
        // bmRequestType 0x40: vendor, host-to-device; wIndex 1 = interface A.
        self.handle
            .write_control(0x40, request, value, 1, &[], Duration::from_millis(500))?;
        Ok(())
    }

    /// Drops anything buffered on either side of the FIFO.
    pub fn purge(&self) -> anyhow::Result<()> {
        self.vendor_request(REQUEST_RESET, RESET_PURGE_RX)?;
        self.vendor_request(REQUEST_RESET, RESET_PURGE_TX)?;
        Ok(())
    }

    /// Writes the whole buffer to the FIFO.
    pub fn write_all(&mut self, mut bytes: &[u8]) -> anyhow::Result<()> {
        while !bytes.is_empty() {
            let n = self
                .handle
                .write_bulk(self.write_ep, bytes, Duration::from_millis(500))?;
            anyhow::ensure!(n > 0, "FTDI bulk write made no progress");
            bytes = bytes.get(n..).unwrap_or_default();
        }
        Ok(())
    }

    /// Reads whatever payload is available within `timeout`, stripping the
    /// two modem-status bytes that lead every USB packet. Returns an empty
    /// vec on timeout — the FIFO simply had nothing to say.
    pub fn read_available(&mut self, timeout: Duration) -> anyhow::Result<Vec<u8>> {
        let mut raw = vec![0u8; self.max_packet_size * 64];
        let n = match self.handle.read_bulk(self.read_ep, &mut raw, timeout) {
            Ok(n) => n,
            Err(rusb::Error::Timeout) => 0,
            Err(e) => return Err(e.into()),
        };
        let mut payload = Vec::with_capacity(n);
        let received = raw.get(..n).unwrap_or_default();
        for packet in received.chunks(self.max_packet_size) {
            if let Some(data) = packet.get(2..) {
                payload.extend_from_slice(data);
            }
        }
        Ok(payload)
    }
}

fn find_device(serial: Option<&str>) -> anyhow::Result<Device<GlobalContext>> {
    let mut seen = Vec::new();
    for device in rusb::devices()?.iter() {
        let Ok(desc) = device.device_descriptor() else {
            continue;
        };
        if desc.vendor_id() != FTDI_VENDOR_ID
            || ![FTDI_FT245RL_PRODUCT_ID, FTDI_FT240X_PRODUCT_ID].contains(&desc.product_id())
        {
            continue;
        }
        let device_serial = device
            .open()
            .ok()
            .and_then(|h| h.read_serial_number_string_ascii(&desc).ok());
        match (serial, &device_serial) {
            (None, _) => return Ok(device),
            (Some(wanted), Some(found)) if wanted == found => return Ok(device),
            _ => seen.push(device_serial.unwrap_or_else(|| "<unreadable>".to_string())),
        }
    }
    match serial {
        Some(wanted) => {
            anyhow::bail!("no P3-ROC FTDI device with serial '{wanted}' (found serials: {seen:?})")
        }
        None => anyhow::bail!(
            "no P3-ROC FTDI device found (vendor 0x0403, product 0x6001/0x6015). \
             Is the controller plugged in and does the user have USB permissions \
             (udev rule or plugdev group)?"
        ),
    }
}
