use std::{num::NonZeroU64, str::FromStr, time::Duration};

#[derive(Clone, Copy, Debug)]
pub(crate) struct DeadlineSeconds(NonZeroU64);

impl DeadlineSeconds {
    pub(crate) fn get(self) -> u64 {
        self.0.get()
    }
}

impl FromStr for DeadlineSeconds {
    type Err = &'static str;
    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let seconds = value
            .parse::<NonZeroU64>()
            .map_err(|_| "deadline must be a positive integer")?;
        std::time::Instant::now()
            .checked_add(Duration::from_secs(seconds.get()))
            .ok_or("deadline is too large for this platform")?;
        Ok(Self(seconds))
    }
}
