use super::*;

#[test]
fn bind_address_validation_requires_remote_opt_in() {
    assert!(validate_bind_address("127.0.0.1:8787".parse().unwrap(), false).is_ok());
    assert!(validate_bind_address("[::1]:8787".parse().unwrap(), false).is_ok());
    assert!(validate_bind_address("192.168.1.5:8787".parse().unwrap(), false).is_err());
    assert!(validate_bind_address("0.0.0.0:8787".parse().unwrap(), true).is_ok());
    assert!(validate_bind_address("192.168.1.5:8787".parse().unwrap(), true).is_ok());
}
