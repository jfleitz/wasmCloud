//! The P3-ROC device task: a dedicated OS thread that owns the FTDI handle.
//!
//! libusb handles are not meaningfully shareable and the FIFO must be read
//! continuously (switch events arrive unrequested), so all bus traffic is
//! serialized through one thread. Host-side interface methods talk to it over
//! an mpsc command channel; inbound switch events fan out to per-subscriber
//! tokio channels backing the `stream<fast-event>`s handed to guests.
//!
//! The thread also owns the connection lifecycle: it (re)connects with
//! backoff, runs the libpinproc init dance (quiesce events → chip-ID verify
//! with the FPGA unlock pattern → driver globals → watchdog → per-switch
//! notify rules → events on), and tickles the watchdog so coils die fast if
//! the host does.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::ftdi::FtdiTransport;
use super::proto::{
    self, P_ROC_CHIP_ID, P3_ROC_CHIP_ID, RxMessage, RxParser, SwitchRule, SwitchTransition,
};

/// How long a register read may wait for its response words.
const READ_RESPONSE_TIMEOUT: Duration = Duration::from_millis(250);
/// Poll granularity of the service loop when idle.
const IDLE_READ_TIMEOUT: Duration = Duration::from_millis(2);
/// Backoff between reconnect attempts.
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

/// Driver groups configured at init, identity-mapped to PDB banks (two banks
/// per PD-16, so 16 groups cover board addresses A0-A7 = drivers 0-127).
/// Banks 16+ can't be scheduled by the FPGA's group logic at all.
const PDB_GROUPS: u8 = 16;
/// Capacity of each subscriber's event channel; a consumer this far behind
/// is cut off (the WIT contract: re-subscribe and re-sync).
pub const SUBSCRIBER_CAPACITY: usize = 1024;

/// Errors surfaced to interface methods (mapped onto WIT `fast-error`).
#[derive(Clone, Debug)]
pub enum HwError {
    NotConnected,
    Timeout,
    Protocol(String),
    Io(String),
}

/// Hardware events fanned out to `subscribe-events` streams.
#[derive(Clone, Copy, Debug)]
pub enum HwEvent {
    Switch {
        switch_id: u8,
        closed: bool,
        timestamp_us: u64,
    },
    LinkUp,
    LinkDown,
}

/// Commands from interface methods to the device thread.
pub enum HwCommand {
    /// Write a pre-encoded word burst; resolves once the FIFO accepted it.
    Write(Vec<u32>, tokio::sync::oneshot::Sender<Result<(), HwError>>),
    /// Read `words` words starting at `addr` of `select`.
    Read {
        select: u32,
        addr: u32,
        words: u32,
        resp: tokio::sync::oneshot::Sender<Result<Vec<u32>, HwError>>,
    },
}

/// Device-thread configuration, resolved from plugin builder + interface
/// config before the thread starts.
#[derive(Clone, Debug)]
pub struct DeviceConfig {
    /// FTDI serial number to bind to; `None` takes the first P-ROC-family
    /// device.
    pub serial: Option<String>,
    /// Installed switches; determines the initial notify-rule sweep and the
    /// size of `read-switch-state`'s bitmap. Multiple of 32, at most 256.
    pub switch_count: u16,
    /// Firmware watchdog reset time; 0 disables the watchdog.
    pub watchdog_ms: u16,
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            serial: None,
            switch_count: 64,
            watchdog_ms: 1000,
        }
    }
}

/// Shared handle between the plugin (host methods) and the device thread.
pub struct DeviceShared {
    pub cmd_tx: mpsc::Sender<HwCommand>,
    pub subscribers: Arc<Mutex<Vec<tokio::sync::mpsc::Sender<HwEvent>>>>,
    pub connected: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl DeviceShared {
    /// Spawns the device thread and returns the shared handle.
    pub fn spawn(config: DeviceConfig) -> anyhow::Result<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let subscribers = Arc::new(Mutex::new(Vec::new()));
        let connected = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::new(AtomicBool::new(false));

        let thread = {
            let subscribers = Arc::clone(&subscribers);
            let connected = Arc::clone(&connected);
            let shutdown = Arc::clone(&shutdown);
            std::thread::Builder::new()
                .name("p3roc-io".to_string())
                .spawn(move || {
                    DeviceTask {
                        config,
                        cmd_rx,
                        subscribers,
                        connected,
                        shutdown,
                        parser: RxParser::default(),
                    }
                    .run()
                })?
        };

        Ok(Self {
            cmd_tx,
            subscribers,
            connected,
            shutdown,
            thread: Mutex::new(Some(thread)),
        })
    }

    /// Registers a new event subscriber, seeding it with the current link
    /// state so late subscribers know where they stand.
    pub fn subscribe(&self) -> tokio::sync::mpsc::Receiver<HwEvent> {
        let (tx, rx) = tokio::sync::mpsc::channel(SUBSCRIBER_CAPACITY);
        if self.connected.load(Ordering::Relaxed) {
            let _ = tx.try_send(HwEvent::LinkUp);
        }
        super::lock_unpoisoned(&self.subscribers).push(tx);
        rx
    }

    /// Signals the device thread to stop and waits for it to wind down
    /// (disabling outputs and the watchdog on the way out).
    pub fn stop(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = super::lock_unpoisoned(&self.thread).take() {
            let _ = handle.join();
        }
    }
}

struct DeviceTask {
    config: DeviceConfig,
    cmd_rx: mpsc::Receiver<HwCommand>,
    subscribers: Arc<Mutex<Vec<tokio::sync::mpsc::Sender<HwEvent>>>>,
    connected: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    parser: RxParser,
}

impl DeviceTask {
    fn run(mut self) {
        while !self.shutdown.load(Ordering::Relaxed) {
            match self.connect_and_init() {
                Ok(transport) => {
                    self.connected.store(true, Ordering::Relaxed);
                    self.fan_out(HwEvent::LinkUp);
                    tracing::info!("P3-ROC link up");

                    let mut session = Session {
                        transport,
                        last_tickle: Instant::now(),
                        watchdog_ms: self.config.watchdog_ms,
                    };
                    let err = self.serve(&mut session);
                    self.connected.store(false, Ordering::Relaxed);

                    if self.shutdown.load(Ordering::Relaxed) {
                        session.quiesce();
                        return;
                    }
                    tracing::warn!(error = %err, "P3-ROC link lost; reconnecting");
                    self.fan_out(HwEvent::LinkDown);
                }
                Err(e) => {
                    tracing::debug!(error = %e, "P3-ROC not reachable; retrying");
                    self.sleep_with_command_drain(RECONNECT_DELAY);
                }
            }
        }
    }

    /// Services commands and inbound traffic until an IO error or shutdown.
    fn serve(&mut self, session: &mut Session) -> anyhow::Error {
        loop {
            if self.shutdown.load(Ordering::Relaxed) {
                return anyhow::anyhow!("shutdown requested");
            }

            // Commands first: coil latency matters more than event latency.
            loop {
                match self.cmd_rx.try_recv() {
                    Ok(cmd) => {
                        if let Err(e) = self.handle_command(session, cmd) {
                            return e;
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        return anyhow::anyhow!("command channel closed");
                    }
                }
            }

            // Inbound traffic (blocks up to IDLE_READ_TIMEOUT, which paces
            // the loop).
            match session.transport.read_available(IDLE_READ_TIMEOUT) {
                Ok(bytes) => self.ingest(&bytes),
                Err(e) => return e,
            }

            if let Err(e) = session.tickle_watchdog_if_due() {
                return e;
            }
        }
    }

    fn handle_command(&mut self, session: &mut Session, cmd: HwCommand) -> anyhow::Result<()> {
        match cmd {
            HwCommand::Write(words, resp) => {
                let result = session.write_words(&words);
                let failed = result.is_err();
                let _ = resp.send(result.as_ref().map(|_| ()).map_err(io_err));
                if failed {
                    return result;
                }
            }
            HwCommand::Read {
                select,
                addr,
                words,
                resp,
            } => match self.read_registers(session, select, addr, words) {
                Ok(data) => {
                    let _ = resp.send(Ok(data));
                }
                Err(ReadError::Timeout) => {
                    let _ = resp.send(Err(HwError::Timeout));
                }
                Err(ReadError::Io(e)) => {
                    let _ = resp.send(Err(HwError::Io(e.to_string())));
                    return Err(e);
                }
            },
        }
        Ok(())
    }

    /// Writes a read request and pumps the bus until its response arrives,
    /// fanning out any events that interleave.
    fn read_registers(
        &mut self,
        session: &mut Session,
        select: u32,
        addr: u32,
        words: u32,
    ) -> Result<Vec<u32>, ReadError> {
        session
            .write_words(&[proto::reg_request(select, addr, words)])
            .map_err(ReadError::Io)?;
        let deadline = Instant::now() + READ_RESPONSE_TIMEOUT;
        loop {
            let bytes = session
                .transport
                .read_available(IDLE_READ_TIMEOUT)
                .map_err(ReadError::Io)?;
            self.parser.extend(&bytes);
            while let Some(msg) = self.parser.next_message() {
                match msg {
                    RxMessage::Requested { words, .. } => return Ok(words),
                    RxMessage::Unrequested(word) => self.dispatch_event(word),
                }
            }
            if Instant::now() > deadline {
                return Err(ReadError::Timeout);
            }
        }
    }

    fn ingest(&mut self, bytes: &[u8]) {
        self.parser.extend(bytes);
        while let Some(msg) = self.parser.next_message() {
            match msg {
                RxMessage::Unrequested(word) => self.dispatch_event(word),
                RxMessage::Requested { header, .. } => {
                    // No read in flight — a stale response from before a
                    // timeout. Drop it.
                    tracing::debug!(header, "dropping unsolicited requested-data message");
                }
            }
        }
    }

    fn dispatch_event(&self, word: u32) {
        let Some(event) = proto::decode_event(word) else {
            return;
        };
        let Ok(switch_id) = u8::try_from(event.switch_num) else {
            tracing::warn!(
                switch = event.switch_num,
                "switch number exceeds wpf:hardware's u8 switch-id; event dropped"
            );
            return;
        };
        let timestamp_us = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64;
        self.fan_out(HwEvent::Switch {
            switch_id,
            closed: event.closed,
            timestamp_us,
        });
    }

    /// Sends `event` to every live subscriber, dropping any that are closed
    /// or have fallen `SUBSCRIBER_CAPACITY` events behind.
    fn fan_out(&self, event: HwEvent) {
        super::lock_unpoisoned(&self.subscribers).retain(|tx| match tx.try_send(event) {
            Ok(()) => true,
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => false,
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!("event subscriber fell behind; closing its stream");
                false
            }
        });
    }

    /// While disconnected, keep answering commands with `not-connected`
    /// instead of letting callers hang.
    fn sleep_with_command_drain(&self, total: Duration) {
        let deadline = Instant::now() + total;
        while Instant::now() < deadline && !self.shutdown.load(Ordering::Relaxed) {
            match self.cmd_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(HwCommand::Write(_, resp)) => {
                    let _ = resp.send(Err(HwError::NotConnected));
                }
                Ok(HwCommand::Read { resp, .. }) => {
                    let _ = resp.send(Err(HwError::NotConnected));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// Opens the FTDI device and brings the FPGA to a known, event-emitting
    /// state — the same sequence libpinproc's `PRDevice::Open` + machine
    /// reset performs, minus the P-ROC-only DMD path.
    fn connect_and_init(&mut self) -> anyhow::Result<FtdiTransport> {
        let mut transport = FtdiTransport::open(self.config.serial.as_deref())?;
        self.parser.clear();

        // Quiesce: if a previous run left events enabled, stop the flood
        // before trying to talk. Ignored by an uninitialized FPGA.
        transport.write_all(&proto::words_to_bytes(&proto::host_events_enable(false)))?;
        std::thread::sleep(Duration::from_millis(20));
        transport.purge()?;

        // Chip-ID verify, sending the FTDI-unlock pattern once if the FPGA
        // doesn't answer (cold power-on state).
        let chip_id = self.verify_chip_id(&mut transport)?;
        match chip_id {
            P3_ROC_CHIP_ID => tracing::info!("P3-ROC detected (chip id 0xF33DB33F)"),
            P_ROC_CHIP_ID => tracing::warn!(
                "P-ROC (not P3-ROC) detected; driver/switch numbering may not match \
                 this plugin's PDB assumptions"
            ),
            other => anyhow::bail!("unrecognized chip id 0x{other:08X}"),
        }

        let mut init = Vec::new();
        // PDB bring-up, in pyprocgame PDBConfig's order: outputs disabled
        // while polarities and groups are programmed, then enabled.
        init.extend(proto::driver_global_config(false));
        // Known-off, active-high state for every schedulable driver, so
        // nothing latches on the moment outputs enable.
        for driver in 0..PDB_GROUPS * 8 {
            init.extend(proto::driver_update(driver, proto::DriverState::off()));
        }
        // Identity group map: group N schedules PDB bank N, making driver
        // numbers board*16 + bank*8 + output (PD-16 board A3 = drivers
        // 48-63). Active groups are refreshed onto the PD-16 serial bus
        // continuously — absent boards ignore the traffic, and present
        // boards *need* it or their comm watchdog (D14 on a PD-16) disables
        // outputs. Group 0's register is also the global-config register
        // (libpinproc shares the address), so groups go first and the
        // global writes below settle it.
        for group in 0..PDB_GROUPS {
            init.extend(proto::driver_group_config(group, group, true));
        }
        // Direct outputs on, active-high (PDB boards).
        init.extend(proto::driver_global_config(true));
        // Watchdog armed so coils drop if the host dies.
        if self.config.watchdog_ms > 0 {
            init.extend(proto::watchdog_config(true, self.config.watchdog_ms));
        }
        // Notify-only rules for every installed switch (debounced open and
        // close), so the FPGA reports all transitions to the host.
        for switch_num in 0..self.config.switch_count.min(256) {
            let switch_num = switch_num as u8;
            for transition in [
                SwitchTransition::ClosedDebounced,
                SwitchTransition::OpenDebounced,
            ] {
                init.extend(proto::switch_rule_update(&SwitchRule {
                    switch_num,
                    transition,
                    notify_host: true,
                    reload_active: false,
                    driver: None,
                }));
            }
        }
        init.extend(proto::host_events_enable(true));
        transport.write_all(&proto::words_to_bytes(&init))?;

        Ok(transport)
    }

    fn verify_chip_id(&mut self, transport: &mut FtdiTransport) -> anyhow::Result<u32> {
        for attempt in 0..6 {
            transport.write_all(&proto::words_to_bytes(&[proto::reg_request(
                proto::MANAGER_SELECT,
                proto::REG_CHIP_ID_ADDR,
                4,
            )]))?;

            let deadline = Instant::now() + Duration::from_millis(300);
            while Instant::now() < deadline {
                let bytes = transport.read_available(Duration::from_millis(20))?;
                self.parser.extend(&bytes);
                while let Some(msg) = self.parser.next_message() {
                    // words: [chip-id, version/revision, watchdog, dips]
                    if let RxMessage::Requested { words, .. } = msg
                        && let &[chip_id, ver_rev, _watchdog, _dips] = words.as_slice()
                    {
                        let version = ver_rev >> 16;
                        let revision = ver_rev & 0xFFFF;
                        tracing::debug!(version, revision, "P3-ROC FPGA firmware");
                        return Ok(chip_id);
                    }
                }
            }

            if attempt == 0 {
                // Cold FPGA: send the FTDI-interface unlock pattern, then retry.
                tracing::debug!("no chip-id response; sending FPGA init pattern");
                transport.write_all(&proto::words_to_bytes(&[
                    proto::INIT_PATTERN_A,
                    proto::INIT_PATTERN_B,
                ]))?;
            }
            self.parser.clear();
            transport.purge()?;
            std::thread::sleep(Duration::from_millis(100));
        }
        anyhow::bail!("P3-ROC did not answer chip-id verification");
    }
}

fn io_err(e: &anyhow::Error) -> HwError {
    HwError::Io(e.to_string())
}

enum ReadError {
    Timeout,
    Io(anyhow::Error),
}

struct Session {
    transport: FtdiTransport,
    last_tickle: Instant,
    watchdog_ms: u16,
}

impl Session {
    fn write_words(&mut self, words: &[u32]) -> anyhow::Result<()> {
        self.transport.write_all(&proto::words_to_bytes(words))
    }

    fn tickle_watchdog_if_due(&mut self) -> anyhow::Result<()> {
        if self.watchdog_ms == 0 {
            return Ok(());
        }
        // Tickle at a quarter of the reset time — generous margin over USB.
        if self.last_tickle.elapsed() >= Duration::from_millis(u64::from(self.watchdog_ms) / 4) {
            self.write_words(&proto::watchdog_config(true, self.watchdog_ms))?;
            self.last_tickle = Instant::now();
        }
        Ok(())
    }

    /// Best-effort safe state on clean shutdown: outputs off, watchdog off,
    /// events off.
    fn quiesce(&mut self) {
        let mut words = proto::driver_global_config(false);
        words.extend(proto::watchdog_config(false, 0));
        words.extend(proto::host_events_enable(false));
        if let Err(e) = self.write_words(&words) {
            tracing::debug!(error = %e, "failed to quiesce P3-ROC on shutdown");
        }
    }
}
