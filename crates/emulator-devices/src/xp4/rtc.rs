//! Shared seconds/timebase mechanism and explicit register-access laws.
use serde::{Deserialize, Serialize};

// Field names and ordering preserve the three-byte peer's checkpoint encoding.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Counter {
    pub(crate) seconds: u32,
    pub(crate) anchor: u64,
    pub(crate) running: bool,
    pub(crate) ticks_per_second: u64,
}
impl Counter {
    pub fn new(seconds: u32, running: bool, ticks_per_second: u64) -> Result<Self, String> {
        if !(1..=1_000_000_000).contains(&ticks_per_second) {
            return Err("Invalid RTC timebase".into());
        }
        Ok(Self {
            seconds,
            anchor: 0,
            running,
            ticks_per_second,
        })
    }
    pub fn counter(&self, tick: u64) -> Result<u32, String> {
        let elapsed = tick
            .checked_sub(self.anchor)
            .ok_or("Power RTC clock moved backwards")?;
        Ok(self.seconds.wrapping_add(if self.running {
            (elapsed / self.ticks_per_second) as u32
        } else {
            0
        }))
    }
    fn running(&mut self, running: bool, tick: u64) -> Result<(), String> {
        if self.running != running {
            self.seconds = self.counter(tick)?;
            self.anchor = tick;
            self.running = running;
        }
        Ok(())
    }
    fn load(&mut self, seconds: u32, tick: u64) {
        self.seconds = seconds;
        self.anchor = tick;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectRegisters {
    pub control: u8,
    pub low: u8,
    pub high: u8,
    pub running_value: u16,
}
impl DirectRegisters {
    pub fn validate(&self) -> Result<(), String> {
        // This legacy peer has one established law. Keeping its admitted map
        // fixed also preserves the meaning of its existing checkpoint shape.
        if *self
            != (Self {
                control: 32,
                low: 34,
                high: 35,
                running_value: 0x8000,
            })
        {
            return Err("Unsupported direct power RTC register law".into());
        }
        Ok(())
    }
    pub fn owns(&self, register: u8) -> bool {
        [self.control, self.low, self.high].contains(&register)
    }
    pub fn read(&self, counter: &Counter, register: u8, tick: u64) -> Result<u16, String> {
        if register == self.control {
            Ok(if counter.running {
                self.running_value
            } else {
                0
            })
        } else if register == self.low {
            Ok(counter.counter(tick)? as u16)
        } else if register == self.high {
            Ok((counter.counter(tick)? >> 16) as u16)
        } else {
            Err("Unknown direct power RTC register".into())
        }
    }
    pub fn write(
        &self,
        counter: &mut Counter,
        register: u8,
        value: u16,
        tick: u64,
    ) -> Result<(), String> {
        if register == self.control {
            if value != 0 && value != self.running_value {
                return Err("Unsupported power RTC control bits".into());
            }
            counter.running(value != 0, tick)
        } else if register == self.low || register == self.high {
            if counter.running {
                return Err("Power RTC counter write while running".into());
            }
            let seconds = if register == self.low {
                (counter.seconds & 0xffff0000) | u32::from(value)
            } else {
                (counter.seconds & 0xffff) | u32::from(value) << 16
            };
            counter.load(seconds, tick);
            Ok(())
        } else {
            Err("Unknown direct power RTC register".into())
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StagedConfig {
    pub control: u8,
    pub read_low: u8,
    pub read_high: u8,
    pub stage_low: u8,
    pub stage_high: u8,
    pub commit_value: u16,
    pub valid_mask: u16,
    pub initial_control: u16,
    pub initial_seconds: u32,
    pub ticks_per_second: u64,
}
impl StagedConfig {
    pub fn validate(&self) -> Result<(), String> {
        let roles = [
            self.control,
            self.read_low,
            self.read_high,
            self.stage_low,
            self.stage_high,
        ];
        let unique: std::collections::BTreeSet<_> = roles.into_iter().collect();
        if unique.len() != roles.len()
            || roles.iter().any(|r| *r > 127)
            || self.valid_mask.count_ones() != 1
            || self.commit_value & self.valid_mask == 0
            || self.initial_control != 0
            || self.initial_seconds != 0
        {
            return Err("Unsupported staged RTC register law or initial state".into());
        }
        Counter::new(0, false, self.ticks_per_second).map(|_| ())
    }
    pub fn owns(&self, register: u8) -> bool {
        [
            self.control,
            self.read_low,
            self.read_high,
            self.stage_low,
            self.stage_high,
        ]
        .contains(&register)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StagedClock {
    pub config: StagedConfig,
    pub counter: Counter,
    staged: u32,
    halves: u8,
    committed: bool,
}
impl StagedClock {
    pub fn new(config: StagedConfig) -> Result<Self, String> {
        config.validate()?;
        Ok(Self {
            counter: Counter::new(config.initial_seconds, false, config.ticks_per_second)?,
            config,
            staged: 0,
            halves: 0,
            committed: false,
        })
    }
    pub fn read(&self, register: u8, tick: u64) -> Result<u16, String> {
        let c = &self.config;
        if register == c.control {
            if self.committed {
                // The load operation establishes the seconds value, but its
                // command/selector-bit readback is not established.
                return Err("Staged RTC control readback after commit is not modeled".into());
            }
            Ok(c.initial_control)
        } else if register == c.read_low {
            Ok(self.counter.counter(tick)? as u16)
        } else if register == c.read_high {
            Ok((self.counter.counter(tick)? >> 16) as u16)
        } else {
            Err("Staged RTC staging-register readback is not modeled".into())
        }
    }
    pub fn write(&mut self, register: u8, value: u16, tick: u64) -> Result<(), String> {
        let c = &self.config;
        if register == c.stage_low {
            self.staged = self.staged & 0xffff0000 | u32::from(value);
            self.halves |= 1;
        } else if register == c.stage_high {
            self.staged = self.staged & 0xffff | u32::from(value) << 16;
            self.halves |= 2;
        } else if register == c.control && value == c.commit_value {
            if self.halves != 3 {
                return Err("Staged RTC commit requires both seconds halves".into());
            }
            self.counter.load(self.staged, tick);
            self.counter.running(true, tick)?;
            self.committed = true;
            self.halves = 0;
        } else {
            return Err(format!(
                "Unsupported staged RTC write register{register} value0x{value:04x}"
            ));
        }
        Ok(())
    }
    pub fn validate_restore(&self, current: &Self) -> Result<(), String> {
        if self.config != current.config
            || self.counter.ticks_per_second != self.config.ticks_per_second
            || self.halves > 3
            || self.counter.running != self.committed
        {
            return Err("Invalid staged RTC checkpoint or configuration".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> StagedConfig {
        StagedConfig {
            control: 100,
            read_low: 101,
            read_high: 102,
            stage_low: 103,
            stage_high: 104,
            commit_value: 0x202,
            valid_mask: 2,
            initial_control: 0,
            initial_seconds: 0,
            ticks_per_second: 10,
        }
    }
    #[test]
    fn staged_load_uses_the_shared_counter_and_refuses_unknown_control_semantics() {
        let mut clock = StagedClock::new(config()).unwrap();
        assert_eq!(clock.read(100, 0).unwrap(), 0);
        clock.write(103, 0x5678, 3).unwrap();
        assert!(clock.write(100, 0x202, 4).is_err());
        assert_eq!(clock.counter.counter(5).unwrap(), 0);
        clock.write(104, 0x1234, 5).unwrap();
        clock.write(100, 0x202, 30).unwrap();
        assert_eq!(clock.read(101, 40).unwrap(), 0x5679);
        assert_eq!(clock.read(102, 40).unwrap(), 0x1234);
        assert!(
            clock
                .read(100, 40)
                .unwrap_err()
                .contains("readback after commit")
        );
        for value in [0, 1, 2, 4, 0x200, 0x206] {
            assert!(clock.write(100, value, 41).is_err());
        }
        assert!(clock.write(42, 0, 41).is_err());
        clock.write(103, 0xffff, 50).unwrap();
        clock.write(104, 0xffff, 51).unwrap();
        assert_eq!(clock.counter.counter(50).unwrap(), 0x1234567a);
        clock.write(100, 0x202, 57).unwrap();
        assert_eq!(clock.counter.counter(67).unwrap(), 0);
    }
    #[test]
    fn staged_checkpoint_preserves_partial_load_and_timebase() {
        let mut clock = StagedClock::new(config()).unwrap();
        clock.write(103, 0x9876, 2).unwrap();
        let bytes = serde_json::to_vec(&clock).unwrap();
        let mut resumed: StagedClock = serde_json::from_slice(&bytes).unwrap();
        resumed.validate_restore(&clock).unwrap();
        resumed.write(104, 0x1234, 3).unwrap();
        resumed.write(100, 0x202, 9).unwrap();
        assert_eq!(resumed.counter.counter(29).unwrap(), 0x12349878);
        let before = serde_json::to_value(&resumed).unwrap();
        assert_eq!(resumed.counter.counter(29).unwrap(), 0x12349878);
        assert_eq!(serde_json::to_value(&resumed).unwrap(), before);
        let mut other = config();
        other.ticks_per_second = 11;
        assert!(
            resumed
                .validate_restore(&StagedClock::new(other).unwrap())
                .is_err()
        );
    }
}
