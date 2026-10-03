{ lib, buildHomeAssistantComponent }:
buildHomeAssistantComponent {
  owner = "mjc";
  domain = "gafctl";
  version = (builtins.fromJSON (builtins.readFile ../custom_components/gafctl/manifest.json)).version;
  src = lib.fileset.toSource {
    root = ../.;
    fileset = ../custom_components/gafctl;
  };
}
