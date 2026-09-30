#[path = "request_session.rs"]
mod request_session;

#[path = "btleplug_session.rs"]
mod implementation;

pub(super) use implementation::query_peripheral;

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_gatt_queries_use_btleplug() {
        assert_eq!(super::implementation::BACKEND, "btleplug");
    }
}
