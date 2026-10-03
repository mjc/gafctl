{
  lib,
  pkgs,
  module,
  homeAssistant,
}:
let
  evaluate =
    settings:
    (lib.nixosSystem {
      system = pkgs.stdenv.hostPlatform.system;
      modules = [
        module
        {
          services.gafctl = settings;
          services.home-assistant.customComponents = [ homeAssistant ];
          system.stateVersion = "26.05";
          boot.isContainer = true;
          fileSystems."/" = {
            device = "none";
            fsType = "tmpfs";
          };
        }
      ];
    }).config;
  enabled = settings: evaluate ({ enable = true; } // settings);
  valid = config: builtins.all (entry: entry.assertion) config.assertions;
  defaults = evaluate { };
  empty = enabled { };
  bluetooth = enabled { bluetooth.deviceId = "test-peripheral"; };
  account = passwordFile: {
    enable = true;
    username = "test-account";
    inherit passwordFile;
  };
  cloudSettings = {
    quickconnect = account "/run/secrets/cloud";
  };
  mqttSettings = {
    mqtt = account "/run/secrets/mqtt" // {
      host = "localhost";
      discovery = true;
    };
  };
  cloud = enabled cloudSettings;
  mqtt = enabled mqttSettings;
  mixed = enabled (
    cloudSettings
    // mqttSettings
    // {
      bluetooth.deviceId = "test-peripheral";
      listenAddress = "::";
      allowRemote = true;
      openFirewall = true;
    }
  );
  serviceFixture = {
    fixtureDirectory = "/run/gafctl-nix-check";
    stateDirectory = "/var/lib/gafctl-nix-check";
    unit = "gafctl-nix-check.service";
    broker = "gafctl-nix-check-broker.service";
    username = "test-account";
    httpPort = 19787;
    mqttPort = 19883;
  };
  service = enabled {
    port = serviceFixture.httpPort;
    mqtt = mqttSettings.mqtt // {
      host = "127.0.0.1";
      port = serviceFixture.mqttPort;
      username = serviceFixture.username;
      passwordFile = "${serviceFixture.fixtureDirectory}/password";
    };
  };
  serviceText =
    lib.replaceStrings
      [ "/var/lib/gafctl/" "StateDirectory=gafctl\n" ]
      [
        "${serviceFixture.stateDirectory}/"
        "StateDirectory=${baseNameOf serviceFixture.stateDirectory}\n"
      ]
      service.systemd.units."gafctl.service".text;
  checks = {
    homeAssistant =
      empty.services.home-assistant.customComponents == [ homeAssistant ]
      && homeAssistant.domain == "gafctl"
      && homeAssistant.isHomeAssistantComponent;
    disabled = !(defaults.systemd.services ? gafctl);
    defaults =
      valid empty
      && empty.systemd.services.gafctl.serviceConfig.User == "gafctl"
      && empty.systemd.services.gafctl.serviceConfig.StateDirectoryMode == "0700"
      && empty.networking.firewall.allowedTCPPorts == [ ]
      && !empty.hardware.bluetooth.enable;
    bluetooth =
      valid bluetooth && bluetooth.hardware.bluetooth.enable && bluetooth.services.dbus.packages != [ ];
    cloud =
      valid cloud
      && !cloud.hardware.bluetooth.enable
      &&
        cloud.systemd.services.gafctl.serviceConfig.LoadCredential
        == [ "quickconnect-password:/run/secrets/cloud" ]
      && !(cloud.systemd.services.gafctl.environment ? GAFCTL_QUICKCONNECT_WRITES_ENABLED);
    mqtt =
      valid mqtt
      && !mqtt.hardware.bluetooth.enable
      &&
        mqtt.systemd.services.gafctl.serviceConfig.LoadCredential == [ "mqtt-password:/run/secrets/mqtt" ];
    mixed =
      valid mixed
      && mixed.hardware.bluetooth.enable
      && builtins.elem 8787 mixed.networking.firewall.allowedTCPPorts;
    remoteRejected =
      !valid (enabled {
        listenAddress = "0.0.0.0";
      });
    firewallRejected =
      !valid (enabled {
        openFirewall = true;
      });
    cloudCredentialRejected =
      !valid (enabled {
        quickconnect.enable = true;
      });
    mqttCredentialRejected =
      !valid (enabled {
        mqtt.enable = true;
      });
    storeSecretRejected =
      !valid (enabled {
        quickconnect = account "/nix/store/secret";
      });
    relativeSecretRejected =
      !valid (enabled {
        mqtt = mqttSettings.mqtt // {
          passwordFile = "secret";
        };
      });
    writesRejected =
      !valid (enabled {
        quickconnect.writesEnabled = true;
      });
    emptyAddressRejected =
      !(builtins.tryEval (enabled { listenAddress = ""; }).services.gafctl.listenAddress).success;
  };
in
assert lib.assertMsg (builtins.all (value: value) (
  builtins.attrValues checks
)) "gafctl NixOS configuration checks failed: ${builtins.toJSON checks}";
(pkgs.writeText "gafctl-module-checks.json" (builtins.toJSON checks)).overrideAttrs (_: {
  passthru.service =
    pkgs.runCommandLocal "gafctl-nix-check"
      {
        unit = pkgs.writeText serviceFixture.unit serviceText;
        fixture = pkgs.writeText "gafctl-nix-check-fixture.json" (builtins.toJSON serviceFixture);
      }
      ''
        mkdir -p "$out"
        cp "$unit" "$out/${serviceFixture.unit}"
        cp "$fixture" "$out/fixture.json"
        for file in "$out/${serviceFixture.unit}" "$out/fixture.json"; do
          test -f "$file"
          test ! -L "$file"
        done
      '';
})
