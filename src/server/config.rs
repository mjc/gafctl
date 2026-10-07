use std::{net::SocketAddr, path::PathBuf};

use crate::quickconnect::{AccountRole, Credentials};
use crate::service::quickconnect::QuickConnectRuntimeConfig;
use anyhow::{Context, Result, bail};

pub(crate) struct ServerConfig {
    pub(crate) device_id: Option<String>,
    pub(crate) identity_store: Option<PathBuf>,
    pub(crate) address: SocketAddr,
    pub(crate) allow_remote: bool,
    #[cfg(feature = "mqtt")]
    pub(crate) mqtt_config: Option<crate::mqtt::MqttConfig>,
    pub(crate) quickconnect_config: Option<QuickConnectRuntimeConfig>,
}

impl ServerConfig {
    pub(super) fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.address.ip().is_loopback() || self.allow_remote,
            "non-loopback API binding requires --allow-remote"
        );
        anyhow::ensure!(
            self.identity_store.is_some()
                || (self.device_id.is_none() && self.quickconnect_config.is_none()),
            "configured devices require --identity-store"
        );
        Ok(())
    }
}

pub(super) fn validate_mqtt_ownership(
    registry: &crate::backend::DeviceRegistry,
    ble_enabled: bool,
    account_id: Option<&str>,
    discovery_enabled: bool,
) -> Result<()> {
    anyhow::ensure!(
        !registry.mqtt_ownership_required(ble_enabled, account_id) || discovery_enabled,
        "persisted MQTT ownership requires a configured broker and --mqtt-discovery"
    );
    Ok(())
}

pub(super) fn read_quickconnect_config(
    role: &str,
    writes_enabled: bool,
) -> Result<Option<QuickConnectRuntimeConfig>> {
    let username = environment_value("GAFCTL_QUICKCONNECT_USERNAME")?;
    let password = environment_value("GAFCTL_QUICKCONNECT_PASSWORD")?;
    let password_file = environment_value("GAFCTL_QUICKCONNECT_PASSWORD_FILE")?.map(PathBuf::from);
    quickconnect_config_from(username, password, password_file, role, writes_enabled)
}

pub(super) fn environment_value(name: &str) -> Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) if value.is_empty() => bail!("{name} must not be empty"),
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => bail!("{name} must be valid UTF-8"),
    }
}

fn quickconnect_config_from(
    username: Option<String>,
    password: Option<String>,
    password_file: Option<PathBuf>,
    role: &str,
    writes_enabled: bool,
) -> Result<Option<QuickConnectRuntimeConfig>> {
    let role = match role {
        "contractor" => AccountRole::Contractor,
        "consumer" => AccountRole::Consumer,
        _ => bail!("GAFCTL_QUICKCONNECT_ROLE must be contractor or consumer"),
    };
    let configured = username.is_some() || password.is_some() || password_file.is_some();
    if !configured {
        anyhow::ensure!(
            !writes_enabled,
            "QuickConnect writes require account credentials"
        );
        return Ok(None);
    }
    let Some(username) = username
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    else {
        bail!("QuickConnect username and one password source are required")
    };
    anyhow::ensure!(
        password.is_none() || password_file.is_none(),
        "configure only one QuickConnect password source"
    );
    let password = match (password, password_file) {
        (Some(password), None) if !password.is_empty() => password,
        (None, Some(path)) => read_private_secret(&path)?,
        _ => bail!("QuickConnect username and one password source are required"),
    };
    let role_name = match role {
        AccountRole::Contractor => "contractor",
        AccountRole::Consumer => "consumer",
    };
    Ok(Some(QuickConnectRuntimeConfig {
        credentials: Credentials::new(username.clone(), password, role),
        account_id: format!("{role_name}:{username}"),
        writes_enabled,
    }))
}

fn read_private_secret(path: &std::path::Path) -> Result<String> {
    let metadata = std::fs::metadata(path).context("could not read QuickConnect password file")?;
    anyhow::ensure!(
        metadata.is_file(),
        "QuickConnect password path is not a file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        anyhow::ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "QuickConnect password file must not be accessible by group or others"
        );
    }
    let password =
        std::fs::read_to_string(path).context("could not read QuickConnect password file")?;
    let password = password.strip_suffix('\n').unwrap_or(&password);
    let password = password.strip_suffix('\r').unwrap_or(password);
    anyhow::ensure!(!password.is_empty(), "QuickConnect password file is empty");
    Ok(password.to_owned())
}

#[cfg(feature = "mqtt")]
pub(super) fn mqtt_config(
    host: Option<String>,
    port: u16,
    username: Option<String>,
    password: Option<String>,
    discovery_enabled: bool,
) -> Result<Option<crate::mqtt::MqttConfig>> {
    match (host, username, password) {
        (Some(host), Some(username), Some(password))
            if !host.is_empty() && !username.is_empty() && !password.is_empty() =>
        {
            Ok(Some(crate::mqtt::MqttConfig {
                host,
                port,
                username,
                password,
                discovery_enabled,
            }))
        }
        (None, None, None) if !discovery_enabled => Ok(None),
        (None, None, None) => bail!("MQTT discovery requires MQTT broker credentials"),
        _ => bail!("MQTT host, username, and GAFCTL_MQTT_PASSWORD must be configured together"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_config() -> ServerConfig {
        ServerConfig {
            device_id: None,
            identity_store: None,
            address: "127.0.0.1:8787".parse().unwrap(),
            allow_remote: false,
            #[cfg(feature = "mqtt")]
            mqtt_config: None,
            quickconnect_config: None,
        }
    }

    #[test]
    fn bind_address_validation_requires_remote_opt_in() {
        [
            ("127.0.0.1:8787", false, true),
            ("[::1]:8787", false, true),
            ("192.168.1.5:8787", false, false),
            ("0.0.0.0:8787", true, true),
            ("192.168.1.5:8787", true, true),
        ]
        .into_iter()
        .for_each(|(address, allow_remote, valid)| {
            let config = ServerConfig {
                address: address.parse().unwrap(),
                allow_remote,
                ..server_config()
            };
            assert_eq!(config.validate().is_ok(), valid, "{address}");
        });
    }

    #[test]
    fn restored_mqtt_ownership_requires_an_available_discovery_publisher() {
        use crate::backend::DeviceRegistry;
        use crate::model::{DeviceId, EntitySource};

        let (_directory, path) = crate::test_support::identity_store_fixture();
        let mut registry = DeviceRegistry::load(&path).unwrap();
        registry.register_configured_ble();
        registry
            .set_entity_sources(
                &DeviceId::configured_ble(),
                EntitySource::Mqtt,
                EntitySource::Mqtt,
            )
            .unwrap();
        let saved = std::fs::read(&path).unwrap();
        let registry = DeviceRegistry::load(&path).unwrap();
        let config = ServerConfig {
            device_id: Some("synthetic-ble-id".into()),
            identity_store: Some(path.clone()),
            ..server_config()
        };
        assert_eq!(
            validate_mqtt_ownership(&registry, config.device_id.is_some(), None, false)
                .unwrap_err()
                .to_string(),
            "persisted MQTT ownership requires a configured broker and --mqtt-discovery"
        );
        assert!(validate_mqtt_ownership(&registry, false, None, false).is_ok());
        assert!(validate_mqtt_ownership(&registry, true, None, true).is_ok());
        assert_eq!(std::fs::read(path).unwrap(), saved);
    }

    #[test]
    fn restored_cloud_mqtt_ownership_is_scoped_to_the_configured_account() {
        use crate::backend::DeviceRegistry;
        use crate::model::EntitySource;

        let (_directory, path) = crate::test_support::identity_store_fixture();
        let mut registry = DeviceRegistry::load(&path).unwrap();
        let ids = registry
            .reconcile_quickconnect(
                "consumer:account",
                &[crate::test_support::cloud_device("provider", "Fan")],
            )
            .unwrap();
        registry
            .set_entity_sources(&ids[0], EntitySource::Mqtt, EntitySource::Mqtt)
            .unwrap();
        let saved = std::fs::read(&path).unwrap();
        let registry = DeviceRegistry::load(&path).unwrap();
        assert!(
            validate_mqtt_ownership(&registry, false, Some("consumer:account"), false).is_err()
        );
        assert!(validate_mqtt_ownership(&registry, false, Some("consumer:account"), true).is_ok());
        assert!(validate_mqtt_ownership(&registry, false, Some("consumer:other"), false).is_ok());
        assert!(validate_mqtt_ownership(&registry, false, None, false).is_ok());
        assert_eq!(std::fs::read(path).unwrap(), saved);
    }

    #[test]
    fn quickconnect_is_optional_without_credentials() {
        assert!(
            quickconnect_config_from(None, None, None, "contractor", false)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn configured_backends_require_a_persistent_identity_store() {
        [(false, false), (true, false), (false, true), (true, true)]
            .into_iter()
            .for_each(|(ble, cloud)| {
                [false, true].into_iter().for_each(|store| {
                    let config = ServerConfig {
                        device_id: ble.then(|| "synthetic-ble-id".into()),
                        identity_store: store.then(|| PathBuf::from("identities.json")),
                        quickconnect_config: cloud.then(|| {
                            quickconnect_config_from(
                                Some("account".into()),
                                Some("secret".into()),
                                None,
                                "consumer",
                                false,
                            )
                            .unwrap()
                            .unwrap()
                        }),
                        ..server_config()
                    };
                    assert_eq!(
                        config.validate().is_ok(),
                        store || (!ble && !cloud),
                        "BLE={ble}, QuickConnect={cloud}, identity store={store}"
                    );
                });
            });
    }

    #[test]
    fn quickconnect_rejects_partial_or_ambiguous_credentials() {
        [
            quickconnect_config_from(Some("account".into()), None, None, "contractor", false),
            quickconnect_config_from(None, Some("secret".into()), None, "contractor", false),
            quickconnect_config_from(
                Some("account".into()),
                Some("secret".into()),
                Some(PathBuf::from("/secret/file")),
                "contractor",
                false,
            ),
            quickconnect_config_from(None, None, None, "contractor", true),
        ]
        .into_iter()
        .for_each(|result| assert!(result.is_err()));
    }

    #[test]
    fn quickconnect_uses_private_password_file_and_keeps_writes_disabled_by_default() {
        use std::os::unix::fs::PermissionsExt;

        let (_directory, path) = crate::test_support::identity_store_fixture();
        let password_path = path.with_file_name("password");
        std::fs::write(&password_path, "private-token\n").unwrap();
        std::fs::set_permissions(&password_path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let config = quickconnect_config_from(
            Some(" account ".into()),
            None,
            Some(password_path),
            "consumer",
            false,
        )
        .unwrap()
        .unwrap();

        assert!(!config.writes_enabled);
        assert_eq!(config.account_id, "consumer:account");
        assert!(!format!("{:?}", config.credentials).contains("private-token"));
    }

    #[test]
    fn quickconnect_password_files_reject_group_or_other_access() {
        use std::os::unix::fs::PermissionsExt;

        let (_directory, path) = crate::test_support::identity_store_fixture();
        let password_path = path.with_file_name("password");
        std::fs::write(&password_path, "private-token").unwrap();
        std::fs::set_permissions(&password_path, std::fs::Permissions::from_mode(0o640)).unwrap();

        let result = quickconnect_config_from(
            Some("account".into()),
            None,
            Some(password_path),
            "consumer",
            false,
        );

        assert!(result.is_err());
    }

    #[test]
    fn quickconnect_writes_require_a_separate_explicit_gate() {
        let config = quickconnect_config_from(
            Some("account".into()),
            Some("private-token".into()),
            None,
            "contractor",
            true,
        )
        .unwrap()
        .unwrap();

        assert!(config.writes_enabled);
        assert!(!format!("{:?}", config.credentials).contains("private-token"));
    }

    #[test]
    #[cfg(feature = "mqtt")]
    fn serve_requires_both_mqtt_credentials_when_push_is_enabled() {
        assert!(
            mqtt_config(
                Some("192.168.1.5".into()),
                1883,
                Some("gafctl".into()),
                None,
                false,
            )
            .is_err()
        );
    }

    #[test]
    #[cfg(feature = "mqtt")]
    fn mqtt_discovery_cannot_be_enabled_without_broker_credentials() {
        assert!(mqtt_config(None, 1883, None, None, true).is_err());
        assert!(
            mqtt_config(None, 1883, None, None, false)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    #[cfg(feature = "mqtt")]
    fn mqtt_discovery_can_be_selected_with_broker_credentials() {
        let config = mqtt_config(
            Some("192.168.1.5".into()),
            1883,
            Some("gafctl".into()),
            Some("secret".into()),
            true,
        )
        .unwrap()
        .unwrap();

        assert!(config.discovery_enabled);
    }
}
