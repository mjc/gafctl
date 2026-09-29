#[path = "request_session.rs"]
mod request_session;

#[path = "btleplug_session.rs"]
mod implementation;

pub(super) use implementation::query_peripheral;
