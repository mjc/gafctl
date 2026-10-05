use serde::{Deserialize, Serialize};

macro_rules! bounded_whole {
    ($(#[$doc:meta])* $name:ident, $range:expr, $message:literal) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
        #[serde(try_from = "u16", into = "u16")]
        pub struct $name(u16);

        impl $name {
            pub const fn value(self) -> u16 {
                self.0
            }
        }

        impl TryFrom<u16> for $name {
            type Error = &'static str;
            fn try_from(value: u16) -> Result<Self, Self::Error> {
                ($range).contains(&value).then_some(Self(value)).ok_or($message)
            }
        }

        impl From<$name> for u16 {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}

bounded_whole!(
    /// Automatic target in whole degrees Fahrenheit (90–120).
    AutomaticTemperatureF, 90..=120,
    "temperature must be 90..120 whole degrees Fahrenheit"
);
bounded_whole!(
    /// Automatic humidity target in whole percent (30–80).
    AutomaticHumidityPercent, 30..=80,
    "humidity must be 30..80 whole percent"
);
bounded_whole!(
    /// Original controller timer in whole minutes (1–360), or zero to clear.
    LegacyTimerMinutes, 0..=360,
    "timer must be 0..360 whole minutes"
);
