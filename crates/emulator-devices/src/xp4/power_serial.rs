//! Bounded physical power-controller SPI register protocol, not a CPU harness.
//! 10a09fc clocks a 24-bit frame; 10a1d40 supplies the six-bit register address.
use super::{
    gpio::SharedPins,
    rtc::{Counter, DirectRegisters},
    spi::Input,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
pub struct PowerSerial {
    pins: SharedPins,
    port: usize,
    pin: usize,
    active_high: bool,
    idle: bool,
    inputs: BTreeMap<u8, u16>,
    registers: BTreeMap<(bool, u8), u16>,
    edge: u64,
    phase: u8,
    header: u8,
    high: u8,
    response: u16,
    rtc: Counter,
    rtc_registers: DirectRegisters,
}
impl PowerSerial {
    pub fn new(
        pins: SharedPins,
        port: usize,
        pin: usize,
        active_high: bool,
        idle: bool,
        inputs: BTreeMap<u8, u16>,
        rtc_ticks_per_second: u64,
        rtc_registers: DirectRegisters,
    ) -> Result<Self, String> {
        if inputs.iter().any(|(k, v)| (3..=6).contains(k) && *v > 1023)
            || port >= 16
            || pin >= 16
            || inputs
                .keys()
                .any(|k| !matches!(*k, 0..=6 | 16 | 32 | 34 | 35))
            || [0, 1, 2, 3, 32, 34, 35]
                .iter()
                .any(|n| !inputs.contains_key(n))
            || inputs.get(&32).is_some_and(|v| !matches!(*v, 0 | 0x8000))
            || !(1..=1_000_000_000).contains(&rtc_ticks_per_second)
        {
            return Err(
                "Power SPI needs explicit ADC/status inputs and a physical chip select".into(),
            );
        }
        rtc_registers.validate()?;
        let rtc = Counter::new(
            u32::from(inputs[&34]) | u32::from(inputs[&35]) << 16,
            inputs[&32] != 0,
            rtc_ticks_per_second,
        )?;
        Ok(Self {
            pins,
            port,
            pin,
            active_high,
            idle,
            inputs,
            registers: BTreeMap::new(),
            edge: 0,
            phase: 0,
            header: 0,
            high: 0,
            response: 0,
            rtc,
            rtc_registers,
        })
    }
    fn writable(register: u8) -> bool {
        matches!(register, 2 | 18 | 32 | 34 | 35 | 38 | 48..=55)
    }
}
impl Input for PowerSerial {
    fn exchange(&mut self, bits: u32, tx: u16, tick: u64) -> Result<u16, String> {
        let (level, edge) = self.pins.lock().unwrap().output(self.port, self.pin);
        if edge != self.edge {
            self.phase = 0;
            self.edge = edge;
        }
        if level != self.active_high {
            return Ok(if self.idle {
                u16::MAX >> (16 - bits)
            } else {
                0
            });
        }
        if bits != 8 {
            return Err("Power SPI requires eight-bit serial words".into());
        }
        let tx = tx as u8;
        match self.phase {
            0 => {
                self.header = tx;
                let register = tx >> 2;
                let bank = tx & 2 != 0;
                if bank && self.rtc_registers.owns(register) {
                    return Err("Power RTC second bank is not modeled".into());
                }
                if tx & 1 != 0 {
                    if !Self::writable(register) {
                        return Err(format!("Unsupported power SPI write register{register}"));
                    }
                    self.response = 0;
                } else if self.rtc_registers.owns(register) {
                    self.response = self.rtc_registers.read(&self.rtc, register, tick)?;
                } else if Self::writable(register) {
                    self.response = *self
                        .registers
                        .get(&(bank, register))
                        .or_else(|| self.inputs.get(&register))
                        .unwrap_or(&0);
                } else if let Some(v) = self.inputs.get(&register) {
                    self.response = *v;
                } else {
                    return Err(format!("Unsupported power SPI read register{register}"));
                }
                self.phase = 1;
                Ok(0)
            }
            1 => {
                self.high = tx;
                self.phase = 2;
                Ok(self.response >> 8)
            }
            _ => {
                if self.header & 1 != 0 {
                    let register = self.header >> 2;
                    let value = u16::from(self.high) << 8 | u16::from(tx);
                    if self.rtc_registers.owns(register) {
                        self.rtc_registers
                            .write(&mut self.rtc, register, value, tick)?;
                    } else {
                        if register == 38 && value & 0x8000 != 0 {
                            return Err("Power RTC alarm interrupt is not modeled".into());
                        }
                        self.registers
                            .insert((self.header & 2 != 0, register), value);
                    }
                }
                self.phase = 0;
                Ok(self.response & 255)
            }
        }
    }
    fn save(&self) -> Value {
        json!({"kind":"power-registers","port":self.port,"pin":self.pin,"active_high":self.active_high,
        "idle":self.idle,"inputs":self.inputs,"registers":self.registers.iter().map(|(&(b,r),&v)|(b,r,v)).collect::<Vec<_>>(),
        "phase":self.phase,"header":self.header,"high":self.high,"response":self.response,"edge":self.edge,"rtc":self.rtc})
    }
    fn restore(&mut self, value: &Value) -> Result<(), String> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct State {
            kind: String,
            port: usize,
            pin: usize,
            active_high: bool,
            idle: bool,
            inputs: BTreeMap<u8, u16>,
            registers: Vec<(bool, u8, u16)>,
            phase: u8,
            header: u8,
            high: u8,
            response: u16,
            edge: u64,
            rtc: Counter,
        }
        let state: State = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
        let registers: BTreeMap<_, _> = state
            .registers
            .iter()
            .map(|(b, r, v)| ((*b, *r), *v))
            .collect();
        if state.kind != "power-registers"
            || state.port != self.port
            || state.pin != self.pin
            || state.active_high != self.active_high
            || state.idle != self.idle
            || state.inputs != self.inputs
            || state.rtc.ticks_per_second != self.rtc.ticks_per_second
            || state.phase > 2
            || registers.len() != state.registers.len()
            || registers
                .keys()
                .any(|(_, r)| !Self::writable(*r) || self.rtc_registers.owns(*r))
        {
            return Err("Invalid power SPI checkpoint or physical input".into());
        }
        self.registers = registers;
        self.phase = state.phase;
        self.header = state.header;
        self.high = state.high;
        self.response = state.response;
        self.edge = state.edge;
        self.rtc = state.rtc;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xp4::gpio::{BASE, Gpio, Pins};
    use crate::{Cx, Device, Host, Record};
    fn rtc_map() -> DirectRegisters {
        DirectRegisters {
            control: 32,
            low: 34,
            high: 35,
            running_value: 0x8000,
        }
    }
    struct Clock;
    impl Host for Clock {
        fn now(&self) -> u64 {
            0
        }
        fn schedule(&mut self, _: usize, _: u64, _: u64) {
            unreachable!()
        }
        fn set_irq(&mut self, _: u32, _: bool) {
            unreachable!()
        }
        fn raise_irq(&mut self, _: usize, _: u32) -> Result<(), String> {
            unreachable!()
        }
        fn ram_read(&self, _: u64, _: &mut [u8]) -> Result<(), String> {
            unreachable!()
        }
        fn ram_write(&mut self, _: u64, _: &[u8]) -> Result<(), String> {
            unreachable!()
        }
        fn record(&mut self, _: Record) {}
    }
    #[test]
    fn rtc_counter_stop_load_start_wrap_and_observation() {
        let map = rtc_map();
        let mut rtc = Counter {
            seconds: 0,
            anchor: 0,
            running: false,
            ticks_per_second: 10,
        };
        map.write(&mut rtc, 34, 0xfffe, 4).unwrap();
        map.write(&mut rtc, 35, 0xffff, 5).unwrap();
        map.write(&mut rtc, 32, 0x8000, 6).unwrap();
        assert_eq!(map.read(&rtc, 34, 15).unwrap(), 0xfffe);
        assert_eq!(map.read(&rtc, 34, 16).unwrap(), 0xffff);
        assert_eq!(rtc.counter(26).unwrap(), 0);
        let saved = serde_json::to_value(&rtc).unwrap();
        for _ in 0..10 {
            assert_eq!(rtc.counter(36).unwrap(), 1);
        }
        assert_eq!(serde_json::to_value(&rtc).unwrap(), saved);
        assert!(map.write(&mut rtc, 34, 12, 36).is_err());
        assert!(map.write(&mut rtc, 32, 1, 36).is_err());
        map.write(&mut rtc, 32, 0, 36).unwrap();
        assert_eq!(rtc.counter(100).unwrap(), 1);
    }
    #[test]
    fn checkpoint_preserves_partial_frame_running_rtc_and_gpio_connection() {
        let pins = Pins::new([0; 16]);
        let inputs = [(0, 0), (1, 0), (2, 0), (3, 512), (32, 0), (34, 0), (35, 0)].into();
        let mut source = PowerSerial::new(
            pins.clone(),
            12,
            5,
            true,
            false,
            inputs,
            54_000_000,
            rtc_map(),
        )
        .unwrap();
        let mut resumed = PowerSerial::new(
            pins.clone(),
            12,
            5,
            true,
            false,
            source.inputs.clone(),
            54_000_000,
            rtc_map(),
        )
        .unwrap();
        let mut gpio = Gpio::new(pins);
        gpio.write(
            &mut Cx::new(&mut Clock, 0, 0, 0),
            BASE + 12 * 64,
            4,
            (1 << 21) | (1 << 5),
        );
        source
            .rtc_registers
            .write(&mut source.rtc, 32, 0x8000, 7)
            .unwrap();
        assert_eq!(
            source.save()["rtc"],
            json!({
                "seconds":0,"anchor":7,"running":true,"ticks_per_second":54_000_000
            })
        );
        source.exchange(8, 18 * 4 + 1, 8).unwrap();
        source.exchange(8, 0x12, 9).unwrap();
        resumed.restore(&source.save()).unwrap();
        for byte in [0x34, 18 * 4, 0, 0] {
            assert_eq!(
                source.exchange(8, byte, 10).unwrap(),
                resumed.exchange(8, byte, 10).unwrap()
            );
        }
        assert_eq!(source.save(), resumed.save());
        assert_eq!(resumed.rtc.counter(54_000_007).unwrap(), 1);
        gpio.write(
            &mut Cx::new(&mut Clock, 0, 0, 0),
            BASE + 12 * 64,
            4,
            1 << 21,
        );
        assert_eq!(resumed.exchange(8, 0, 11).unwrap(), 0);
    }
    #[test]
    fn physical_selection_frames_latches_and_unknown_commands() {
        let pins = Pins::new([0; 16]);
        let mut gpio = Gpio::new(pins.clone());
        let mut peer = PowerSerial::new(
            pins,
            12,
            5,
            true,
            false,
            [(0, 0), (1, 0), (2, 0), (3, 512), (32, 0), (34, 0), (35, 0)].into(),
            54_000_000,
            rtc_map(),
        )
        .unwrap();
        assert_eq!(peer.exchange(8, 12, 0).unwrap(), 0); // Unselected wire is low.
        let mut host = Clock;
        let mut cx = Cx::new(&mut host, 3, 0x765432, 0);
        gpio.write(&mut cx, BASE + 12 * 64, 4, (1 << 21) | (1 << 5));
        assert_eq!(
            [
                peer.exchange(8, 12, 0).unwrap(),
                peer.exchange(8, 0, 0).unwrap(),
                peer.exchange(8, 0, 0).unwrap()
            ],
            [0, 2, 0]
        );
        assert_eq!(
            [
                peer.exchange(8, 18 * 4 + 1, 0).unwrap(),
                peer.exchange(8, 0x12, 0).unwrap(),
                peer.exchange(8, 0x34, 0).unwrap()
            ],
            [0, 0, 0]
        );
        assert_eq!(
            [
                peer.exchange(8, 18 * 4, 0).unwrap(),
                peer.exchange(8, 0, 0).unwrap(),
                peer.exchange(8, 0, 0).unwrap()
            ],
            [0, 0x12, 0x34]
        );
        let saved = peer.save();
        assert_eq!(peer.save(), saved);
        peer.exchange(8, 12, 0).unwrap();
        gpio.write(&mut cx, BASE + 12 * 64, 4, 1 << 21);
        gpio.write(&mut cx, BASE + 12 * 64, 4, (1 << 21) | (1 << 5));
        assert_eq!(peer.exchange(8, 12, 0).unwrap(), 0);
        gpio.write(&mut cx, BASE + 12 * 64, 4, 1 << 21);
        gpio.write(&mut cx, BASE + 12 * 64, 4, (1 << 21) | (1 << 5));
        assert!(peer.exchange(8, 63 * 4, 0).is_err());
        peer.exchange(8, 38 * 4 + 1, 0).unwrap();
        peer.exchange(8, 0x80, 0).unwrap();
        assert!(peer.exchange(8, 0, 0).is_err());
        assert!(cx.refusal().is_none());
    }
}
