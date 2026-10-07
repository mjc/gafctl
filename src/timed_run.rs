use crate::model::LegacyTimerMinutes;
use crate::protocol::{
    AutomaticThresholds, ControlCommand, DeviceSnapshot, FanState, HumidityTenthsPercent, Minutes,
    OperatingMode, TemperatureTenthsF,
};
use serde::{Deserialize, Serialize};

const MINUTE_MS: u64 = 60_000;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct TimerConfiguration {
    pub(crate) duration_minutes: LegacyTimerMinutes,
    pub(crate) run: Option<TimedRun>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(
        mode: OperatingMode,
        fan: FanState,
        remaining: u16,
        original: u16,
    ) -> TimerObservation {
        TimerObservation {
            mode,
            fan,
            remaining,
            original,
            temperature: 1051,
            humidity: 301,
        }
    }

    fn start(initial: &TimerObservation, minutes: u16) -> TimedRun {
        TimedRun::start("fan", minutes.try_into().unwrap(), initial, None, 0).unwrap()
    }

    fn expired() -> TimerObservation {
        observation(OperatingMode::Timer, FanState::Off, 0, 1)
    }

    #[test]
    fn expiry_restores_fractional_automatic_thresholds_but_never_early() {
        let mut run = start(
            &observation(OperatingMode::Automatic, FanState::Off, 0, 0),
            1,
        );
        assert_eq!(run.observe("fan", &expired(), 59_999), TimerAction::Wait);
        assert_eq!(
            run.observe("fan", &expired(), 60_000),
            TimerAction::Restore(ControlCommand::SetAutomaticThresholds(
                AutomaticThresholds {
                    temperature: TemperatureTenthsF::new(1051),
                    humidity: HumidityTenthsPercent::new(301),
                }
            ))
        );
    }

    #[test]
    fn prior_off_stays_off_and_prior_timer_resumes_only_its_remaining_window() {
        let mut off = start(&observation(OperatingMode::Timer, FanState::Off, 0, 0), 1);
        assert_eq!(off.observe("fan", &expired(), 60_000), TimerAction::Cancel);
        let mut timer = start(&observation(OperatingMode::Timer, FanState::On, 3, 5), 1);
        assert_eq!(
            timer.observe("fan", &expired(), 60_000),
            TimerAction::Restore(ControlCommand::SetTimer(Minutes::new(2)))
        );
        assert_eq!(
            timer.observe("fan", &expired(), 180_000),
            TimerAction::Cancel
        );
    }

    #[test]
    fn extension_preserves_original_resume_state_and_restart_preserves_deadline() {
        let initial = start(
            &observation(OperatingMode::Automatic, FanState::On, 0, 0),
            1,
        );
        let active = observation(OperatingMode::Timer, FanState::On, 1, 1);
        let extended = TimedRun::start(
            "fan",
            2.try_into().unwrap(),
            &active,
            Some(&initial),
            30_000,
        )
        .unwrap();
        assert_eq!(extended.resume, ResumeMode::Automatic);
        assert_eq!(extended.expires_at_ms, 150_000);
        let mut restored: TimedRun =
            serde_json::from_str(&serde_json::to_string(&extended).unwrap()).unwrap();
        let mut stopped = expired();
        stopped.original = 2;
        assert_eq!(restored.resume, ResumeMode::Automatic);
        assert_eq!(
            restored.observe("fan", &stopped, 150_000),
            TimerAction::Restore(stopped.automatic_command().unwrap())
        );
    }

    #[test]
    fn ownership_changes_and_early_manual_stop_cancel_restoration() {
        let initial = observation(OperatingMode::Automatic, FanState::Off, 0, 0);
        let run = start(&initial, 2);
        let active = observation(OperatingMode::Timer, FanState::On, 1, 2);
        let mut changed = observation(OperatingMode::Timer, FanState::Off, 0, 2);
        assert_eq!(run.clone().observe("fan", &changed, 1), TimerAction::Cancel);
        assert_eq!(
            run.clone().observe("other", &active, 1),
            TimerAction::Cancel
        );
        changed.temperature += 1;
        assert_eq!(
            run.clone().observe("fan", &changed, 120_000),
            TimerAction::Cancel
        );
        assert_eq!(
            run.clone().observe("fan", &initial, 120_000),
            TimerAction::Cancel
        );
        let mut increasing = run;
        assert_eq!(increasing.observe("fan", &active, 1), TimerAction::Wait);
        assert_eq!(
            increasing.observe(
                "fan",
                &observation(OperatingMode::Timer, FanState::On, 2, 2),
                2
            ),
            TimerAction::Cancel
        );
    }

    #[test]
    fn corrupted_persisted_deadline_cannot_restore() {
        let run = start(
            &observation(OperatingMode::Automatic, FanState::Off, 0, 0),
            1,
        );
        let mut encoded = serde_json::to_value(&run).unwrap();
        encoded["expires_at_ms"] = serde_json::json!(1);
        let mut corrupt: TimedRun = serde_json::from_value(encoded).unwrap();
        assert_eq!(corrupt.observe("fan", &expired(), 1), TimerAction::Cancel);
        assert!(
            serde_json::from_str::<TimerConfiguration>("{\"duration_minutes\":999,\"run\":null}")
                .is_err()
        );
        assert!(
            TimedRun::start(
                "fan",
                1.try_into().unwrap(),
                &observation(OperatingMode::Ota, FanState::Off, 0, 0),
                None,
                0
            )
            .is_none()
        );
    }
}

impl Default for TimerConfiguration {
    fn default() -> Self {
        Self {
            duration_minutes: LegacyTimerMinutes::try_from(360).expect("valid maximum duration"),
            run: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct TimedRun {
    peripheral_id: String,
    started_at_ms: u64,
    expires_at_ms: u64,
    minutes: LegacyTimerMinutes,
    temperature: u16,
    humidity: u16,
    resume: ResumeMode,
    #[serde(skip)]
    last_remaining: Option<u16>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum ResumeMode {
    Automatic,
    Off,
    Timer { expires_at_ms: u64 },
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum TimerAction {
    Wait,
    Cancel,
    Restore(ControlCommand),
}

pub(crate) struct TimerObservation {
    mode: OperatingMode,
    fan: FanState,
    temperature: u16,
    humidity: u16,
    remaining: u16,
    original: u16,
}

impl TimerObservation {
    pub(crate) fn from_snapshot(snapshot: &DeviceSnapshot) -> Option<Self> {
        let mode = snapshot.mode.decoded().ok()?;
        let thresholds = snapshot.thresholds.decoded().ok()?;
        let timer = snapshot.timer.as_ref()?.decoded().ok()?;
        Some(Self {
            mode: mode.mode,
            fan: mode.fan,
            temperature: thresholds.temperature.value(),
            humidity: thresholds.humidity.value(),
            remaining: timer.remaining.value(),
            original: timer.original.value(),
        })
    }

    fn automatic_command(&self) -> Option<ControlCommand> {
        crate::legacy_control::prepare_control(
            crate::model::DeviceCommand::LegacyMode {
                mode: crate::model::LegacyControlMode::Automatic,
            },
            Some(AutomaticThresholds {
                temperature: TemperatureTenthsF::new(self.temperature),
                humidity: HumidityTenthsPercent::new(self.humidity),
            }),
            None,
        )
    }

    fn resume_mode(&self, now_ms: u64) -> Option<ResumeMode> {
        match (self.mode, self.fan) {
            (OperatingMode::Automatic, _) => Some(ResumeMode::Automatic),
            (OperatingMode::Timer, FanState::Off) => Some(ResumeMode::Off),
            (OperatingMode::Timer, FanState::On) if (1..=360).contains(&self.remaining) => {
                Some(ResumeMode::Timer {
                    expires_at_ms: now_ms.checked_add(u64::from(self.remaining) * MINUTE_MS)?,
                })
            }
            (OperatingMode::Timer, FanState::On) | (OperatingMode::Ota, _) => None,
        }
    }
}

impl TimedRun {
    pub(crate) fn start(
        peripheral_id: &str,
        minutes: LegacyTimerMinutes,
        observation: &TimerObservation,
        previous: Option<&Self>,
        now_ms: u64,
    ) -> Option<Self> {
        if minutes.value() == 0 {
            return None;
        }
        observation.automatic_command()?;
        let resume = previous
            .filter(|run| run.matches(peripheral_id, observation) && observation.remaining > 0)
            .map(|run| run.resume)
            .or_else(|| observation.resume_mode(now_ms))?;
        Some(Self {
            peripheral_id: peripheral_id.to_owned(),
            started_at_ms: now_ms,
            expires_at_ms: now_ms.checked_add(u64::from(minutes.value()) * MINUTE_MS)?,
            minutes,
            temperature: observation.temperature,
            humidity: observation.humidity,
            resume,
            last_remaining: None,
        })
    }

    fn matches(&self, peripheral_id: &str, observation: &TimerObservation) -> bool {
        self.minutes.value() > 0
            && self
                .started_at_ms
                .checked_add(u64::from(self.minutes.value()) * MINUTE_MS)
                == Some(self.expires_at_ms)
            && self.peripheral_id == peripheral_id
            && observation.mode == OperatingMode::Timer
            && observation.original == self.minutes.value()
            && observation.temperature == self.temperature
            && observation.humidity == self.humidity
            && observation.remaining <= self.minutes.value()
    }

    pub(crate) fn observe(
        &mut self,
        peripheral_id: &str,
        observation: &TimerObservation,
        now_ms: u64,
    ) -> TimerAction {
        if !self.matches(peripheral_id, observation)
            || now_ms < self.started_at_ms
            || self
                .last_remaining
                .is_some_and(|previous| observation.remaining > previous)
        {
            return TimerAction::Cancel;
        }
        self.last_remaining = Some(observation.remaining);
        if observation.remaining > 0 || observation.fan == FanState::On {
            return TimerAction::Wait;
        }
        if now_ms < self.expires_at_ms {
            return if self.expires_at_ms - now_ms > MINUTE_MS {
                TimerAction::Cancel
            } else {
                TimerAction::Wait
            };
        }
        let command = match self.resume {
            ResumeMode::Automatic => observation.automatic_command(),
            ResumeMode::Off => None,
            ResumeMode::Timer { expires_at_ms } => {
                let remaining = expires_at_ms.saturating_sub(now_ms).div_ceil(MINUTE_MS);
                (remaining > 0).then(|| {
                    ControlCommand::SetTimer(Minutes::new(
                        u16::try_from(remaining).unwrap_or(360).min(360),
                    ))
                })
            }
        };
        command.map_or(TimerAction::Cancel, TimerAction::Restore)
    }
}
