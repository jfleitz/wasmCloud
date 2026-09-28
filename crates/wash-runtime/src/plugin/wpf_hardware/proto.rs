//! P-ROC / P3-ROC wire protocol: word encoding, bus framing, and event
//! decoding.
//!
//! The P3-ROC speaks 32-bit words over an FTDI FIFO. Every word on the wire is
//! big-endian. Outbound traffic is a header word (read request or write burst)
//! optionally followed by payload words; inbound traffic is either *requested*
//! data (echoed header + payload, in response to a read request) or
//! *unrequested* data (header + one event word, pushed by the switch
//! controller).
//!
//! Register map and bit layouts are transcribed from libpinproc
//! (`include/pinproc.h`, `src/PRHardware.cpp` — MIT licensed, Gerry
//! Stellenberg & Adam Preble). Only the subset the wpf `controller` interface
//! needs is carried over: manager regs, driver updates, switch rules, switch
//! state reads, and the watchdog.

/// FTDI vendor/product IDs the P-ROC family enumerates with.
pub const FTDI_VENDOR_ID: u16 = 0x0403;
/// FT245RL (P-ROC, early P3-ROC).
pub const FTDI_FT245RL_PRODUCT_ID: u16 = 0x6001;
/// FT240X (current P3-ROC).
pub const FTDI_FT240X_PRODUCT_ID: u16 = 0x6015;

/// Written once after a cold plug-in to unlock the FPGA's FTDI interface.
pub const INIT_PATTERN_A: u32 = 0x801F_1122;
pub const INIT_PATTERN_B: u32 = 0x3456_78AB;

/// Chip IDs returned from manager register 0.
pub const P_ROC_CHIP_ID: u32 = 0xFEED_BEEF;
pub const P3_ROC_CHIP_ID: u32 = 0xF33D_B33F;

// Header word fields (both directions).
const ADDR_SHIFT: u32 = 0;
const HEADER_LENGTH_SHIFT: u32 = 20;
const COMMAND_SHIFT: u32 = 31;
const COMMAND_MASK: u32 = 0x8000_0000;
const HEADER_LENGTH_MASK: u32 = 0x7FF0_0000;
const MODULE_SELECT_SHIFT: u32 = 16;

// Module selects (P3-ROC).
pub const MANAGER_SELECT: u32 = 0;
pub const SWITCH_CTRL_SELECT: u32 = 2;
pub const DRIVER_CTRL_SELECT: u32 = 3;
pub const STATE_CHANGE_PROC_SELECT: u32 = 4;

// Manager registers.
pub const REG_CHIP_ID_ADDR: u32 = 0;
pub const REG_WATCHDOG_ADDR: u32 = 2;

// Watchdog config word fields.
const WATCHDOG_EXPIRED_SHIFT: u32 = 30;
const WATCHDOG_ENABLE_SHIFT: u32 = 14;

// Driver control address decodes.
const DRIVER_CTRL_DECODE_SHIFT: u32 = 10;
const DRIVER_REG_DECODE: u32 = 0;
const DRIVER_CONFIG_TABLE_DECODE: u32 = 1;
const DRIVER_CONFIG_TABLE_DRIVER_NUM_SHIFT: u32 = 1;

// Driver config word fields (word 1 of a driver update).
const DRIVER_CONFIG_POLARITY_SHIFT: u32 = 8;
const DRIVER_CONFIG_STATE_SHIFT: u32 = 9;
const DRIVER_CONFIG_UPDATE_SHIFT: u32 = 10;
// Driver config word fields (word 2 of a driver update).
const DRIVER_CONFIG_PATTER_ON_TIME_SHIFT: u32 = 16;
const DRIVER_CONFIG_PATTER_OFF_TIME_SHIFT: u32 = 23;
const DRIVER_CONFIG_PATTER_ENABLE_SHIFT: u32 = 30;

// Driver global config word fields.
const DRIVER_GLOBAL_ENABLE_DIRECT_OUTPUTS_SHIFT: u32 = 31;
const DRIVER_GLOBAL_GLOBAL_POLARITY_SHIFT: u32 = 30;
const DRIVER_GLOBAL_START_STROBE_TIME_SHIFT: u32 = 20;

// Driver group config word fields (reg-decode register `group_num`).
const DRIVER_GROUP_DISABLE_STROBE_AFTER_SHIFT: u32 = 11;
const DRIVER_GROUP_ENABLE_INDEX_SHIFT: u32 = 7;
const DRIVER_GROUP_POLARITY_SHIFT: u32 = 1;
const DRIVER_GROUP_ACTIVE_SHIFT: u32 = 0;

// Switch controller / state-change-processor.
pub const P3_ROC_SWITCH_STATE_BASE_ADDR: u32 = 16;
const STATE_CHANGE_CONFIG_ADDR: u32 = 0x1000;
const SWITCH_RULE_NUM_DEBOUNCE_SHIFT: u32 = 9;
const SWITCH_RULE_NUM_STATE_SHIFT: u32 = 8;
const SWITCH_RULE_NUM_TO_ADDR_SHIFT: u32 = 2;
const SWITCH_RULE_RELOAD_ACTIVE_SHIFT: u32 = 31;
const SWITCH_RULE_NOTIFY_HOST_SHIFT: u32 = 23;
const SWITCH_RULE_CHANGE_OUTPUT_SHIFT: u32 = 9;

// V2 event word fields (the P3-ROC FPGA is always version >= 2).
const V2_EVENT_SWITCH_NUM_MASK: u32 = 0x7FF;
const V2_EVENT_SWITCH_STATE_MASK: u32 = 0x1000;
const V2_EVENT_TYPE_MASK: u32 = 0xC000;
const V2_EVENT_TYPE_SHIFT: u32 = 14;
const V2_EVENT_SWITCH_DEBOUNCED_MASK: u32 = 0x2000;
const EVENT_TYPE_SWITCH: u32 = 0;

/// Builds the header word for a register read request.
pub fn reg_request(select: u32, addr: u32, num_words: u32) -> u32 {
    (num_words << HEADER_LENGTH_SHIFT) | (select << MODULE_SELECT_SHIFT) | (addr << ADDR_SHIFT)
}

/// Decodes a read-request word back into `(select, addr, num_words)`.
/// Returns `None` for write bursts (command bit set) or zero-length reads.
/// Used by `send-raw` to recognize a raw read request and route it through
/// the request/response path instead of blind-writing it.
pub fn parse_read_request(word: u32) -> Option<(u32, u32, u32)> {
    if word & COMMAND_MASK != 0 {
        return None;
    }
    let num_words = (word & HEADER_LENGTH_MASK) >> HEADER_LENGTH_SHIFT;
    if num_words == 0 {
        return None;
    }
    let select = (word >> MODULE_SELECT_SHIFT) & 0xF;
    let addr = word & 0xFFFF;
    Some((select, addr, num_words))
}

/// Builds the header word for a write burst of `num_words` payload words.
pub fn burst_header(select: u32, addr: u32, num_words: u32) -> u32 {
    (1 << COMMAND_SHIFT)
        | (num_words << HEADER_LENGTH_SHIFT)
        | (select << MODULE_SELECT_SHIFT)
        | (addr << ADDR_SHIFT)
}

/// One driver (coil) state, mirroring libpinproc's `PRDriverState` for the
/// fields the P3-ROC direct-driver path uses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DriverState {
    /// Enabled (pulsing, held, or pattering) vs. off.
    pub state: bool,
    /// Drive time in ms for a pulse; 0 = on until changed.
    pub output_drive_time: u8,
    /// Patter (software PWM) on/off times in ms, 7 bits each.
    pub patter_on_ms: u8,
    pub patter_off_ms: u8,
    pub patter_enable: bool,
}

impl DriverState {
    pub fn pulse(duration_ms: u8) -> Self {
        Self {
            state: true,
            output_drive_time: duration_ms,
            ..Default::default()
        }
    }

    pub fn hold() -> Self {
        Self {
            state: true,
            ..Default::default()
        }
    }

    pub fn patter(on_ms: u8, off_ms: u8) -> Self {
        Self {
            state: true,
            patter_on_ms: on_ms & 0x7F,
            patter_off_ms: off_ms & 0x7F,
            patter_enable: true,
            ..Default::default()
        }
    }

    pub fn off() -> Self {
        Self::default()
    }

    /// The two config-table payload words for this state. PDB outputs are
    /// active-high, so polarity is always set (matching libpinproc's PDB
    /// machine type).
    fn config_words(self) -> [u32; 2] {
        let word1 = u32::from(self.output_drive_time)
            | (1 << DRIVER_CONFIG_POLARITY_SHIFT)
            | (u32::from(self.state) << DRIVER_CONFIG_STATE_SHIFT)
            | (1 << DRIVER_CONFIG_UPDATE_SHIFT);
        let word2 = (u32::from(self.patter_on_ms & 0x7F) << DRIVER_CONFIG_PATTER_ON_TIME_SHIFT)
            | (u32::from(self.patter_off_ms & 0x7F) << DRIVER_CONFIG_PATTER_OFF_TIME_SHIFT)
            | (u32::from(self.patter_enable) << DRIVER_CONFIG_PATTER_ENABLE_SHIFT);
        [word1, word2]
    }
}

/// Burst that writes `state` into the driver config table for `driver_num`.
/// The P3-ROC forwards the update to the owning PD-16 over its serial bus.
pub fn driver_update(driver_num: u8, state: DriverState) -> Vec<u32> {
    let addr = (DRIVER_CONFIG_TABLE_DECODE << DRIVER_CTRL_DECODE_SHIFT)
        | (u32::from(driver_num) << DRIVER_CONFIG_TABLE_DRIVER_NUM_SHIFT);
    let [word1, word2] = state.config_words();
    vec![burst_header(DRIVER_CTRL_SELECT, addr, 2), word1, word2]
}

/// Burst that writes the driver global config: direct outputs on/off with
/// active-high polarity and a 1-cycle start-strobe time (the values
/// pyprocgame's PDBConfig writes for PDB/PD-16 machines). Everything
/// matrix-related stays zero — the P3-ROC has no direct lamp matrix.
pub fn driver_global_config(enable_outputs: bool) -> Vec<u32> {
    let word = (u32::from(enable_outputs) << DRIVER_GLOBAL_ENABLE_DIRECT_OUTPUTS_SHIFT)
        | (1 << DRIVER_GLOBAL_GLOBAL_POLARITY_SHIFT)
        | (1 << DRIVER_GLOBAL_START_STROBE_TIME_SHIFT);
    vec![burst_header(DRIVER_CTRL_SELECT, 0, 1), word]
}

/// Burst that writes one driver-group config register. Group `group` owns
/// driver numbers `group*8 .. group*8+7` and, while `active`, the FPGA's
/// driver loop continuously schedules them onto PDB bank `enable_index`
/// (PD-16 bank = board*2 + half, so bank 7 = board A3 outputs 8-15). That
/// continuous refresh is also what feeds each PD-16's comm watchdog — with
/// no active group for its banks, a PD-16 lights D14 and disables outputs.
///
/// Without these writes the config-table updates from `driver_update` are
/// acked but never leave the FPGA: libpinproc deliberately writes no group
/// defaults for PDB machines and expects the framework to do it.
pub fn driver_group_config(group: u8, enable_index: u8, active: bool) -> Vec<u32> {
    let addr = (DRIVER_REG_DECODE << DRIVER_CTRL_DECODE_SHIFT) | u32::from(group);
    let word = (1 << DRIVER_GROUP_DISABLE_STROBE_AFTER_SHIFT)
        | (u32::from(enable_index) << DRIVER_GROUP_ENABLE_INDEX_SHIFT)
        | (1 << DRIVER_GROUP_POLARITY_SHIFT)
        | (u32::from(active) << DRIVER_GROUP_ACTIVE_SHIFT);
    vec![burst_header(DRIVER_CTRL_SELECT, addr, 1), word]
}

/// Burst that arms (or disarms) the watchdog. Tickling is the same write
/// repeated before `reset_time_ms` elapses.
pub fn watchdog_config(enable: bool, reset_time_ms: u16) -> Vec<u32> {
    let word = (u32::from(enable) << WATCHDOG_ENABLE_SHIFT) | u32::from(reset_time_ms);
    let _ = WATCHDOG_EXPIRED_SHIFT; // expired flag intentionally never set by the host
    vec![burst_header(MANAGER_SELECT, REG_WATCHDOG_ADDR, 1), word]
}

/// Burst that enables/disables unrequested switch events to the host
/// (state-change-processor config).
pub fn host_events_enable(enable: bool) -> Vec<u32> {
    vec![
        burst_header(STATE_CHANGE_PROC_SELECT, STATE_CHANGE_CONFIG_ADDR, 1),
        u32::from(enable),
    ]
}

/// Which switch transition a rule fires on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwitchTransition {
    ClosedDebounced,
    OpenDebounced,
    ClosedNondebounced,
    OpenNondebounced,
}

impl SwitchTransition {
    fn debounce_bit(self) -> u32 {
        matches!(self, Self::ClosedDebounced | Self::OpenDebounced).into()
    }

    fn open_bit(self) -> u32 {
        matches!(self, Self::OpenDebounced | Self::OpenNondebounced).into()
    }
}

/// A firmware-resident switch rule: what the state-change processor does when
/// `switch_num` makes `transition`.
#[derive(Clone, Copy, Debug)]
pub struct SwitchRule {
    pub switch_num: u8,
    pub transition: SwitchTransition,
    /// Send an unrequested event word to the host.
    pub notify_host: bool,
    /// Throttle re-fires through the firmware reload timer.
    pub reload_active: bool,
    /// Apply `driver` when the rule fires (autofire); `None` writes a
    /// notify-only rule.
    pub driver: Option<(u8, DriverState)>,
}

/// Burst that writes one switch rule slot.
pub fn switch_rule_update(rule: &SwitchRule) -> Vec<u32> {
    let index = (rule.transition.debounce_bit() << SWITCH_RULE_NUM_DEBOUNCE_SHIFT)
        | (rule.transition.open_bit() << SWITCH_RULE_NUM_STATE_SHIFT)
        | u32::from(rule.switch_num);
    let addr = index << SWITCH_RULE_NUM_TO_ADDR_SHIFT;

    let (driver_num, driver_state) = rule.driver.unwrap_or((0, DriverState::off()));
    let [word1, word2] = driver_state.config_words();
    let word3 = (u32::from(rule.reload_active) << SWITCH_RULE_RELOAD_ACTIVE_SHIFT)
        | (u32::from(rule.notify_host) << SWITCH_RULE_NOTIFY_HOST_SHIFT)
        | (u32::from(rule.driver.is_some()) << SWITCH_RULE_CHANGE_OUTPUT_SHIFT)
        | u32::from(driver_num);

    vec![
        burst_header(STATE_CHANGE_PROC_SELECT, addr, 3),
        word1,
        word2,
        word3,
    ]
}

/// Serializes words for the wire (big-endian, matching the FPGA's byte order).
pub fn words_to_bytes(words: &[u32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for word in words {
        bytes.extend_from_slice(&word.to_be_bytes());
    }
    bytes
}

/// A decoded switch event from an unrequested data word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwitchEventWord {
    pub switch_num: u16,
    pub closed: bool,
}

/// Decodes a V2 unrequested event word; `None` for non-switch events
/// (accelerometer etc.), which this plugin ignores.
pub fn decode_event(word: u32) -> Option<SwitchEventWord> {
    let event_type = (word & V2_EVENT_TYPE_MASK) >> V2_EVENT_TYPE_SHIFT;
    if event_type != EVENT_TYPE_SWITCH {
        return None;
    }
    let open = word & V2_EVENT_SWITCH_STATE_MASK != 0;
    let _debounced = word & V2_EVENT_SWITCH_DEBOUNCED_MASK != 0;
    Some(SwitchEventWord {
        switch_num: (word & V2_EVENT_SWITCH_NUM_MASK) as u16,
        closed: !open,
    })
}

/// One fully-reassembled inbound message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RxMessage {
    /// Response to a read request: echoed header + payload words.
    Requested { header: u32, words: Vec<u32> },
    /// Pushed by the FPGA (switch/accelerometer events).
    Unrequested(u32),
}

/// Incremental parser for the inbound byte stream.
///
/// Unlike libpinproc's `SortReturningData` (which assumes a whole message is
/// readable once its header is), this buffers bytes and only emits a message
/// when every word of it has arrived, so short USB reads can never
/// desynchronize the word stream.
#[derive(Default)]
pub struct RxParser {
    buf: Vec<u8>,
}

impl RxParser {
    /// Appends raw bytes from the transport.
    pub fn extend(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Discards any buffered bytes (used when re-syncing after reconnect).
    pub fn clear(&mut self) {
        self.buf.clear();
    }

    fn peek_word(&self, index: usize) -> Option<u32> {
        let start = index * 4;
        let bytes: [u8; 4] = self.buf.get(start..start + 4)?.try_into().ok()?;
        Some(u32::from_be_bytes(bytes))
    }

    /// Pops the next complete message, if one has fully arrived.
    pub fn next_message(&mut self) -> Option<RxMessage> {
        let header = self.peek_word(0)?;
        if header & COMMAND_MASK == 0 {
            // Requested data: header echoes the request, length field counts
            // the payload words that follow.
            let len = ((header & HEADER_LENGTH_MASK) >> HEADER_LENGTH_SHIFT) as usize;
            let mut words = Vec::with_capacity(len);
            for i in 0..len {
                words.push(self.peek_word(1 + i)?);
            }
            self.buf.drain(..(1 + len) * 4);
            Some(RxMessage::Requested { header, words })
        } else {
            // Unrequested data: header + exactly one event word (matching
            // libpinproc's demux).
            let event = self.peek_word(1)?;
            self.buf.drain(..8);
            Some(RxMessage::Unrequested(event))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cross-checked against libpinproc's `CreateRegRequestWord`: the chip-ID
    /// request the reference implementation sends at open.
    #[test]
    fn chip_id_request_matches_libpinproc() {
        assert_eq!(
            reg_request(MANAGER_SELECT, REG_CHIP_ID_ADDR, 4),
            0x0040_0000
        );
    }

    #[test]
    fn parse_read_request_round_trips() {
        let word = reg_request(SWITCH_CTRL_SELECT, P3_ROC_SWITCH_STATE_BASE_ADDR, 2);
        assert_eq!(
            parse_read_request(word),
            Some((SWITCH_CTRL_SELECT, P3_ROC_SWITCH_STATE_BASE_ADDR, 2))
        );
        // Write bursts and zero-length reads are not read requests.
        assert_eq!(parse_read_request(burst_header(0, 2, 1)), None);
        assert_eq!(parse_read_request(0), None);
    }

    /// Cross-checked against `CreateBurstCommand` + `CreateWatchdogConfigBurst`
    /// with enable=true, resetTime=1000: header 0x80100002, data 0x40003E8.
    #[test]
    fn watchdog_burst_matches_libpinproc() {
        assert_eq!(
            watchdog_config(true, 1000),
            vec![0x8010_0002, (1 << 14) | 1000]
        );
    }

    /// Cross-checked against `CreateDriverUpdateBurst` for a 30 ms pulse of
    /// driver 5 with PDB polarity: addr = (1<<10)|(5<<1), words carry
    /// drive-time, polarity, state, and the update bit.
    #[test]
    fn driver_pulse_burst_matches_libpinproc() {
        let burst = driver_update(5, DriverState::pulse(30));
        assert_eq!(burst[0], 0x8023_040A);
        assert_eq!(burst[1], 30 | (1 << 8) | (1 << 9) | (1 << 10));
        assert_eq!(burst[2], 0);
    }

    /// Cross-checked against `CreateDriverUpdateGroupConfigBurst` for group 6
    /// → PDB bank 6 (board A3 outputs 0-7), active, PDB polarity,
    /// disableStrobeAfter: addr = reg-decode | 6, word = strobe-after |
    /// enable-index | polarity | active.
    #[test]
    fn driver_group_burst_matches_libpinproc() {
        assert_eq!(
            driver_group_config(6, 6, true),
            vec![0x8013_0006, (1 << 11) | (6 << 7) | (1 << 1) | 1]
        );
    }

    #[test]
    fn patter_encodes_pwm_fields() {
        let burst = driver_update(0, DriverState::patter(3, 7));
        assert_eq!(burst[2], (3 << 16) | (7 << 23) | (1 << 30));
    }

    /// Cross-checked against `CreateSwitchRuleAddr`/`CreateSwitchUpdateRulesBurst`
    /// for a notify-only closed-debounced rule on switch 8: index =
    /// (1<<9)|8, addr = index<<2, notify bit 23 set, no output change.
    #[test]
    fn notify_rule_matches_libpinproc() {
        let burst = switch_rule_update(&SwitchRule {
            switch_num: 8,
            transition: SwitchTransition::ClosedDebounced,
            notify_host: true,
            reload_active: false,
            driver: None,
        });
        assert_eq!(burst[0], 0x8034_0000 | (((1 << 9) | 8) << 2));
        assert_eq!(burst[3], 1 << 23);
    }

    #[test]
    fn autofire_rule_sets_change_output_and_driver() {
        let burst = switch_rule_update(&SwitchRule {
            switch_num: 3,
            transition: SwitchTransition::ClosedNondebounced,
            notify_host: false,
            reload_active: true,
            driver: Some((9, DriverState::pulse(20))),
        });
        // Non-debounced closed: debounce=0, state=0 -> index is the switch number.
        assert_eq!(burst[0], 0x8034_0000 | (3 << 2));
        assert_eq!(burst[1], 20 | (1 << 8) | (1 << 9) | (1 << 10));
        assert_eq!(burst[3], (1u32 << 31) | (1 << 9) | 9);
    }

    /// V2 event word layout: switch 42 closing, debounced. State bit 12 is
    /// "open", so closed events have it clear.
    #[test]
    fn decodes_v2_switch_events() {
        let closed = (1 << 13) | 42; // debounced, closed, type=0
        assert_eq!(
            decode_event(closed),
            Some(SwitchEventWord {
                switch_num: 42,
                closed: true
            })
        );
        let open = (1 << 13) | (1 << 12) | 42;
        assert_eq!(
            decode_event(open),
            Some(SwitchEventWord {
                switch_num: 42,
                closed: false
            })
        );
        // Accelerometer event type is ignored.
        assert_eq!(decode_event(3 << 14 | 42), None);
    }

    #[test]
    fn parser_reassembles_across_partial_reads() {
        let mut parser = RxParser::default();
        // Requested response: header (len=2) + 2 payload words.
        let header = reg_request(MANAGER_SELECT, REG_CHIP_ID_ADDR, 2);
        let bytes = words_to_bytes(&[header, P3_ROC_CHIP_ID, 0x0002_0014]);

        parser.extend(&bytes[..5]);
        assert_eq!(parser.next_message(), None);
        parser.extend(&bytes[5..9]);
        assert_eq!(parser.next_message(), None);
        parser.extend(&bytes[9..]);
        assert_eq!(
            parser.next_message(),
            Some(RxMessage::Requested {
                header,
                words: vec![P3_ROC_CHIP_ID, 0x0002_0014]
            })
        );
        assert_eq!(parser.next_message(), None);
    }

    #[test]
    fn parser_demuxes_unrequested_events() {
        let mut parser = RxParser::default();
        let event_word = (1 << 13) | 7;
        parser.extend(&words_to_bytes(&[1 << 31, event_word]));
        assert_eq!(
            parser.next_message(),
            Some(RxMessage::Unrequested(event_word))
        );
    }

    #[test]
    fn words_round_trip_big_endian() {
        assert_eq!(words_to_bytes(&[0x801F_1122]), vec![0x80, 0x1F, 0x11, 0x22]);
    }
}
