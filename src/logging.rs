use tracing_subscriber::EnvFilter;

fn default_filter() -> EnvFilter {
    EnvFilter::new("updraft=info")
}

fn filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| default_filter())
}

pub(crate) fn init() {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter())
        .init();
}

#[cfg(test)]
mod tests {
    use super::default_filter;

    #[test]
    fn default_filter_includes_updraft_info_events() {
        assert_eq!(default_filter().to_string(), "updraft=info");
    }
}
