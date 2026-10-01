use serde::{Deserialize, Serialize};

/// Original controller target in whole degrees Fahrenheit (90–120).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "u16", into = "u16")]
pub struct LegacyTemperatureF(u16);

impl LegacyTemperatureF {
    pub const fn value(self) -> u16 {
        self.0
    }
}

impl TryFrom<u16> for LegacyTemperatureF {
    type Error = &'static str;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        (90..=120)
            .contains(&value)
            .then_some(Self(value))
            .ok_or("temperature must be 90..120 whole degrees Fahrenheit")
    }
}

impl From<LegacyTemperatureF> for u16 {
    fn from(value: LegacyTemperatureF) -> Self {
        value.0
    }
}

/// Original controller humidity target in whole percent (30–80).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "u16", into = "u16")]
pub struct LegacyHumidityPercent(u16);

impl LegacyHumidityPercent {
    pub const fn value(self) -> u16 {
        self.0
    }
}

impl TryFrom<u16> for LegacyHumidityPercent {
    type Error = &'static str;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        (30..=80)
            .contains(&value)
            .then_some(Self(value))
            .ok_or("humidity must be 30..80 whole percent")
    }
}

impl From<LegacyHumidityPercent> for u16 {
    fn from(value: LegacyHumidityPercent) -> Self {
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
