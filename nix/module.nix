{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.gafctl;
  inherit (lib) mkEnableOption mkOption types;
  nonempty = types.strMatching ".+";
  optionalString =
    description:
    mkOption {
      type = types.nullOr nonempty;
      default = null;
      inherit description;
    };
  flag =
    description:
    mkOption {
      type = types.bool;
      default = false;
      inherit description;
    };
  accountOptions = name: {
    username = optionalString "${name} account username.";
    passwordFile = optionalString "Absolute runtime path to the ${name} password, outside the Nix store.";
  };
  hasAccount = backend: backend.username != null && credential backend.passwordFile;
  accounts = lib.filterAttrs (_: backend: backend.enable) {
    mqtt = cfg.mqtt;
    quickconnect = cfg.quickconnect;
  };
  credential = value: value != null && lib.hasPrefix "/" value && !lib.hasPrefix "/nix/store/" value;
  loopback =
    cfg.listenAddress == "::1"
    || builtins.match "127\\.[0-9]+\\.[0-9]+\\.[0-9]+" cfg.listenAddress != null;
  bind = "${
    if lib.hasInfix ":" cfg.listenAddress then "[${cfg.listenAddress}]" else cfg.listenAddress
  }:${toString cfg.port}";
  policy = pkgs.runCommand "gafctl-dbus-policy" { } ''
    install -Dm644 ${../packaging/dbus/gafctl.conf} $out/share/dbus-1/system.d/gafctl.conf
  '';
  launcher = pkgs.writeShellScript "gafctl-start" ''
    set -eu
    ${lib.optionalString cfg.mqtt.enable ''
      export GAFCTL_MQTT_PASSWORD="$(cat "$CREDENTIALS_DIRECTORY/mqtt-password")"
    ''}
    exec ${lib.getExe' cfg.package "gafctl-server"} --bind ${lib.escapeShellArg bind} ${lib.optionalString cfg.allowRemote "--allow-remote"}
  '';
in
{
  options.services.gafctl = {
    enable = mkEnableOption "GAF attic fan control";
    package = mkOption {
      type = types.package;
      description = "Package providing gafctl and gafctl-server.";
    };
    bluetooth.deviceId = optionalString "Original controller's peripheral ID from gafctl ble scan.";
    listenAddress = mkOption {
      type = nonempty;
      default = "127.0.0.1";
      description = "IP address for the HTTP listener.";
    };
    port = mkOption {
      type = types.port;
      default = 8787;
      description = "HTTP API port.";
    };
    allowRemote = flag "Allow a listener outside loopback. The API has no authentication.";
    openFirewall = flag "Open the HTTP port in the host firewall.";
    mqtt = accountOptions "MQTT" // {
      enable = mkEnableOption "MQTT state and controls";
      host = optionalString "MQTT broker hostname.";
      port = mkOption {
        type = types.port;
        default = 1883;
        description = "MQTT broker port.";
      };
      discovery = flag "Publish Home Assistant MQTT discovery.";
    };
    quickconnect = accountOptions "QuickConnect" // {
      enable = mkEnableOption "experimental QuickConnect cloud access";
      role = mkOption {
        type = types.enum [
          "consumer"
          "contractor"
        ];
        default = "consumer";
        description = "QuickConnect account role.";
      };
      writesEnabled = flag "Allow experimental QuickConnect settings changes.";
    };
  };
  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = loopback || cfg.allowRemote;
        message = "services.gafctl: a non-loopback listener requires allowRemote.";
      }
      {
        assertion = !cfg.openFirewall || (!loopback && cfg.allowRemote);
        message = "services.gafctl: openFirewall requires a remote listener and allowRemote.";
      }
      {
        assertion = !cfg.mqtt.enable || (cfg.mqtt.host != null && hasAccount cfg.mqtt);
        message = "services.gafctl.mqtt: host, username, and a runtime passwordFile are required.";
      }
      {
        assertion = !cfg.quickconnect.enable || (hasAccount cfg.quickconnect);
        message = "services.gafctl.quickconnect: username and a runtime passwordFile are required.";
      }
      {
        assertion = !cfg.quickconnect.writesEnabled || cfg.quickconnect.enable;
        message = "services.gafctl.quickconnect: writesEnabled requires enable.";
      }
    ];
    users.groups.gafctl = { };
    users.users.gafctl = {
      isSystemUser = true;
      group = "gafctl";
    };
    hardware.bluetooth.enable = lib.mkIf (cfg.bluetooth.deviceId != null) true;
    services.dbus.packages = lib.optional (cfg.bluetooth.deviceId != null) policy;
    networking.firewall.allowedTCPPorts = lib.optional cfg.openFirewall cfg.port;
    systemd.services.gafctl = {
      description = "GAF attic fan control";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [
        "network-online.target"
      ]
      ++ lib.optional (cfg.bluetooth.deviceId != null) "bluetooth.service";
      environment = {
        GAFCTL_IDENTITY_STORE = "/var/lib/gafctl/identities.json";
        SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
      }
      // lib.optionalAttrs (cfg.bluetooth.deviceId != null) { GAFCTL_DEVICE_ID = cfg.bluetooth.deviceId; }
      // lib.optionalAttrs cfg.mqtt.enable {
        GAFCTL_MQTT_HOST = cfg.mqtt.host;
        GAFCTL_MQTT_PORT = toString cfg.mqtt.port;
        GAFCTL_MQTT_USERNAME = cfg.mqtt.username;
      }
      // lib.optionalAttrs (cfg.mqtt.enable && cfg.mqtt.discovery) { GAFCTL_MQTT_DISCOVERY = "true"; }
      // lib.optionalAttrs cfg.quickconnect.enable {
        GAFCTL_QUICKCONNECT_USERNAME = cfg.quickconnect.username;
        GAFCTL_QUICKCONNECT_PASSWORD_FILE = "%d/quickconnect-password";
        GAFCTL_QUICKCONNECT_ROLE = cfg.quickconnect.role;
      }
      // lib.optionalAttrs (cfg.quickconnect.enable && cfg.quickconnect.writesEnabled) {
        GAFCTL_QUICKCONNECT_WRITES_ENABLED = "true";
      };
      serviceConfig = {
        Type = "exec";
        User = "gafctl";
        Group = "gafctl";
        StateDirectory = "gafctl";
        StateDirectoryMode = "0700";
        UMask = "0077";
        LoadCredential = lib.mapAttrsToList (
          name: backend: "${name}-password:${backend.passwordFile}"
        ) accounts;
        ExecStart = launcher;
        Restart = "on-failure";
        RestartSec = 5;
        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        RestrictAddressFamilies = [
          "AF_UNIX"
          "AF_INET"
          "AF_INET6"
        ];
        CapabilityBoundingSet = "";
      };
    };
  };
}
