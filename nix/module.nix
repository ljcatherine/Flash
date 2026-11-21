# NixOS module: `programs.flash.enable = true;`
self:
{ config, lib, pkgs, ... }:

let
  cfg = config.programs.flash;
in
{
  options.programs.flash = {
    enable = lib.mkEnableOption "Flash, the driver updater and PC health tool";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "flash.packages.\${system}.default";
      description = "The Flash package.";
    };
  };

  config = lib.mkIf cfg.enable {
    # The package's polkit policy lets the active user read SMART and memory details
    # without a password, and remembers the administrator password for changes.
    environment.systemPackages = [ cfg.package ];
    security.polkit.enable = true;

    # Firmware updates (LVFS) and drive health. NixOS manages packages and kernel modules
    # declaratively, so Flash shows what to change in configuration.nix for those.
    services.fwupd.enable = lib.mkDefault true;
    services.udisks2.enable = lib.mkDefault true;
  };
}
