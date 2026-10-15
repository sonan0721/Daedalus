use std::time::Duration;

use daedalus_core::Tick;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const BILLION: u128 = 1_000_000_000;

struct ClockAdvanceData {
    first_tick: Option<Tick>,
    requested_ticks: u64,
    executed_ticks: u32,
    deferred_ticks: u64,
    elapsed_nanos: u64,
    input_saturated: bool,
    tick_counter_saturated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FixedClockConfig {
    pub tick_rate_hz: u32,
    pub max_catch_up_ticks: u32,
}

impl Default for FixedClockConfig {
    fn default() -> Self {
        Self {
            tick_rate_hz: 60,
            max_catch_up_ticks: 4,
        }
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ClockConfigError {
    #[error("fixed tick rate must be positive")]
    ZeroTickRate,
    #[error("maximum catch-up tick count must be positive")]
    ZeroCatchUpLimit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ClockAdvance {
    pub first_tick: Option<Tick>,
    pub current_tick: Tick,
    pub requested_ticks: u64,
    pub executed_ticks: u32,
    pub deferred_ticks: u64,
    pub fractional_numerator: u32,
    pub elapsed_nanos: u64,
    pub input_saturated: bool,
    pub tick_counter_saturated: bool,
}

/// An exact rational fixed-rate clock with retained catch-up debt and bounded work per advance.
#[derive(Clone, Debug)]
pub struct FixedClock {
    config: FixedClockConfig,
    current_tick: Tick,
    accumulated_scaled_nanos: u128,
}

impl FixedClock {
    pub fn new(config: FixedClockConfig) -> Result<Self, ClockConfigError> {
        if config.tick_rate_hz == 0 {
            return Err(ClockConfigError::ZeroTickRate);
        }
        if config.max_catch_up_ticks == 0 {
            return Err(ClockConfigError::ZeroCatchUpLimit);
        }
        Ok(Self {
            config,
            current_tick: Tick(0),
            accumulated_scaled_nanos: 0,
        })
    }

    #[must_use]
    pub const fn config(&self) -> FixedClockConfig {
        self.config
    }

    #[must_use]
    pub const fn current_tick(&self) -> Tick {
        self.current_tick
    }

    /// Add elapsed wall time, execute at most the configured number of fixed ticks, and retain debt.
    pub fn advance(&mut self, elapsed: Duration) -> ClockAdvance {
        let scaled_elapsed = elapsed
            .as_nanos()
            .checked_mul(u128::from(self.config.tick_rate_hz));
        let Some(scaled_elapsed) = scaled_elapsed else {
            self.accumulated_scaled_nanos = u128::MAX;
            return self.consume_ready_ticks(elapsed.as_nanos(), true);
        };
        let next = self.accumulated_scaled_nanos.checked_add(scaled_elapsed);
        let input_saturated = next.is_none();
        self.accumulated_scaled_nanos = next.unwrap_or(u128::MAX);
        self.consume_ready_ticks(elapsed.as_nanos(), input_saturated)
    }

    /// Advance one explicit simulation step, consuming retained catch-up debt first when present.
    pub fn step_tick(&mut self) -> ClockAdvance {
        let tick_before = self.current_tick;
        let available_to_execute = u64::from(self.config.max_catch_up_ticks);
        let mut requested_ticks =
            (self.accumulated_scaled_nanos / BILLION).min(u128::from(u64::MAX)) as u64;
        if requested_ticks == 0 {
            requested_ticks = 1;
        }
        let available_counter = u64::MAX.saturating_sub(tick_before.0);
        let executed = 1_u64.min(available_to_execute).min(available_counter);
        if executed == 0 {
            return self.diagnostics(ClockAdvanceData {
                first_tick: None,
                requested_ticks,
                executed_ticks: 0,
                deferred_ticks: 0,
                elapsed_nanos: 0,
                input_saturated: false,
                tick_counter_saturated: true,
            });
        }
        if self.accumulated_scaled_nanos >= BILLION {
            self.accumulated_scaled_nanos -= BILLION;
        }
        self.current_tick = Tick(tick_before.0 + executed);
        self.diagnostics(ClockAdvanceData {
            first_tick: Some(Tick(tick_before.0 + 1)),
            requested_ticks,
            executed_ticks: executed as u32,
            deferred_ticks: (self.accumulated_scaled_nanos / BILLION).min(u128::from(u64::MAX))
                as u64,
            elapsed_nanos: 0,
            input_saturated: false,
            tick_counter_saturated: false,
        })
    }

    fn consume_ready_ticks(&mut self, elapsed_nanos: u128, input_saturated: bool) -> ClockAdvance {
        let tick_before = self.current_tick;
        let ready = (self.accumulated_scaled_nanos / BILLION).min(u128::from(u64::MAX)) as u64;
        let available_counter = u64::MAX.saturating_sub(tick_before.0);
        let execute = ready
            .min(u64::from(self.config.max_catch_up_ticks))
            .min(available_counter);
        self.accumulated_scaled_nanos = self
            .accumulated_scaled_nanos
            .saturating_sub(u128::from(execute).saturating_mul(BILLION));
        self.current_tick = Tick(tick_before.0.saturating_add(execute));
        let deferred = (self.accumulated_scaled_nanos / BILLION).min(u128::from(u64::MAX)) as u64;
        let tick_counter_saturated = ready > execute && self.current_tick.0 == u64::MAX;
        let first_tick = (execute > 0).then(|| Tick(tick_before.0 + 1));
        self.diagnostics(ClockAdvanceData {
            first_tick,
            requested_ticks: ready,
            executed_ticks: execute as u32,
            deferred_ticks: deferred,
            elapsed_nanos: elapsed_nanos.min(u128::from(u64::MAX)) as u64,
            input_saturated,
            tick_counter_saturated,
        })
    }

    fn diagnostics(&self, data: ClockAdvanceData) -> ClockAdvance {
        ClockAdvance {
            first_tick: data.first_tick,
            current_tick: self.current_tick,
            requested_ticks: data.requested_ticks,
            executed_ticks: data.executed_ticks,
            deferred_ticks: data.deferred_ticks,
            fractional_numerator: (self.accumulated_scaled_nanos % BILLION) as u32,
            elapsed_nanos: data.elapsed_nanos,
            input_saturated: data.input_saturated,
            tick_counter_saturated: data.tick_counter_saturated,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use daedalus_core::Tick;

    use super::{ClockConfigError, FixedClock, FixedClockConfig};

    #[test]
    fn rational_tick_rate_accumulates_fractional_elapsed_time() {
        let mut clock = FixedClock::new(FixedClockConfig {
            tick_rate_hz: 60,
            max_catch_up_ticks: 4,
        })
        .unwrap();
        let first = clock.advance(Duration::from_millis(16));
        assert_eq!(first.executed_ticks, 0);
        assert_eq!(first.fractional_numerator, 960_000_000);
        let second = clock.advance(Duration::from_millis(1));
        assert_eq!(second.executed_ticks, 1);
        assert_eq!(clock.current_tick(), Tick(1));
        assert_eq!(second.fractional_numerator, 20_000_000);
    }

    #[test]
    fn catch_up_is_bounded_and_debt_is_retained_and_reported() {
        let mut clock = FixedClock::new(FixedClockConfig {
            tick_rate_hz: 60,
            max_catch_up_ticks: 4,
        })
        .unwrap();
        let first = clock.advance(Duration::from_secs(1));
        assert_eq!(first.requested_ticks, 60);
        assert_eq!(first.executed_ticks, 4);
        assert_eq!(first.deferred_ticks, 56);
        let second = clock.advance(Duration::ZERO);
        assert_eq!(second.executed_ticks, 4);
        assert_eq!(second.deferred_ticks, 52);
        assert_eq!(second.current_tick, Tick(8));
    }

    #[test]
    fn manual_steps_consume_backlog_and_tick_numbers_are_monotonic() {
        let mut clock = FixedClock::new(FixedClockConfig::default()).unwrap();
        let advance = clock.advance(Duration::from_millis(50));
        assert_eq!(advance.executed_ticks, 3);
        let step = clock.step_tick();
        assert_eq!(step.first_tick, Some(Tick(4)));
        assert_eq!(step.current_tick, Tick(4));
        assert_eq!(step.deferred_ticks, 0);
        assert_eq!(clock.step_tick().current_tick, Tick(5));
    }

    #[test]
    fn zero_configuration_is_rejected() {
        assert_eq!(
            FixedClock::new(FixedClockConfig {
                tick_rate_hz: 0,
                max_catch_up_ticks: 1,
            })
            .unwrap_err(),
            ClockConfigError::ZeroTickRate
        );
        assert_eq!(
            FixedClock::new(FixedClockConfig {
                tick_rate_hz: 1,
                max_catch_up_ticks: 0,
            })
            .unwrap_err(),
            ClockConfigError::ZeroCatchUpLimit
        );
    }

    #[test]
    fn exhausted_tick_counter_and_accumulator_overflow_are_reported_without_panicking() {
        let mut clock = FixedClock::new(FixedClockConfig::default()).unwrap();
        clock.current_tick = Tick(u64::MAX);
        let saturated = clock.advance(Duration::from_secs(1));
        assert_eq!(saturated.first_tick, None);
        assert_eq!(saturated.executed_ticks, 0);
        assert!(saturated.tick_counter_saturated);

        let mut clock = FixedClock::new(FixedClockConfig::default()).unwrap();
        clock.accumulated_scaled_nanos = u128::MAX - 1;
        let overflow = clock.advance(Duration::from_nanos(1));
        assert!(overflow.input_saturated);
        assert_eq!(overflow.executed_ticks, 4);
    }
}
