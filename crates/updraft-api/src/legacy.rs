use serde::{Deserialize, Serialize};

/// Automatic target in whole degrees Fahrenheit (90–120).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "u16", into = "u16")]
pub struct AutomaticTemperatureF(u16);

impl AutomaticTemperatureF {
    pub const fn value(self) -> u16 {
        self.0
    }
}

impl TryFrom<u16> for AutomaticTemperatureF {
    type Error = &'static str;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        (90..=120)
            .contains(&value)
            .then_some(Self(value))
            .ok_or("temperature must be 90..120 whole degrees Fahrenheit")
    }
}

impl From<AutomaticTemperatureF> for u16 {
    fn from(value: AutomaticTemperatureF) -> Self {
        value.0
    }
}

/// Automatic humidity target in whole percent (30–80).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "u16", into = "u16")]
pub struct AutomaticHumidityPercent(u16);

impl AutomaticHumidityPercent {
    pub const fn value(self) -> u16 {
        self.0
    }
}

impl TryFrom<u16> for AutomaticHumidityPercent {
    type Error = &'static str;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        (30..=80)
            .contains(&value)
            .then_some(Self(value))
            .ok_or("humidity must be 30..80 whole percent")
    }
}

impl From<AutomaticHumidityPercent> for u16 {
    fn from(value: AutomaticHumidityPercent) -> Self {
        value.0
    }
}

/// Original controller timer in whole minutes (1–360), or zero to clear.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "u16", into = "u16")]
pub struct LegacyTimerMinutes(u16);

impl LegacyTimerMinutes {
    pub const fn value(self) -> u16 {
        self.0
    }
}

impl TryFrom<u16> for LegacyTimerMinutes {
    type Error = &'static str;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        (value <= 360)
            .then_some(Self(value))
            .ok_or("timer must be 0..360 whole minutes")
    }
}

impl From<LegacyTimerMinutes> for u16 {
    fn from(value: LegacyTimerMinutes) -> Self {
        value.0
    }
}
