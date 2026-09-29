#[path = "request_session.rs"]
mod request_session;

#[cfg(not(target_os = "linux"))]
#[path = "btleplug_session.rs"]
mod implementation;

#[cfg(target_os = "linux")]
#[path = "linux_session.rs"]
mod implementation;

pub(super) use implementation::query_peripheral;
