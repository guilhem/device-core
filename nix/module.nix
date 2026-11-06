{ config, lib, ... }:

let
  cfg = config.services.device-core;
  guard = "/run/lock/device-core/network";
in
{
  options.services.device-core = {
    enable = lib.mkEnableOption "device-core";
    package = lib.mkOption {
      type = lib.types.package;
      description = "Package providing bin/device-core.";
    };
    captiveDnsAddress = lib.mkOption {
      type = lib.types.nullOr ((lib.types.addCheck lib.types.str (address:
        let
          octets = lib.splitString "." address;
          first = lib.toInt (builtins.head octets);
        in
        builtins.length octets == 4
        && builtins.all (octet:
          builtins.match "(0|[1-9][0-9]{0,2})" octet != null && lib.toInt octet <= 255
        ) octets
        && first != 127 && !(first >= 224 && first <= 239)
        && address != "0.0.0.0" && address != "255.255.255.255"
      )) // {
        description = "concrete unicast IPv4 address without leading-zero octets";
      });
      default = null;
      description = "Captive DNS address for all NetworkManager shared connections; null disables it. Does not affect station DNS or start the Wi-Fi portal.";
    };
    dataDir = lib.mkOption {
      type = (lib.types.addCheck lib.types.externalPath (path:
        builtins.match "(/[A-Za-z0-9._-]+)+" path != null
        && path != builtins.storeDir
        && !lib.hasPrefix "${builtins.storeDir}/" path
        && builtins.all (part: part != "." && part != "..") (lib.splitString "/" path)
      )) // {
        description = "normalized non-root absolute path outside the Nix store with only ASCII letters, digits, dots, underscores, and hyphens in each component";
      };
      default = "/var/lib/device-core";
      description = "Writable absolute path for persistent device-core state.";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.etc = lib.mkIf (cfg.captiveDnsAddress != null) {
      "NetworkManager/dnsmasq-shared.d/device-core-captive.conf".text = ''
        address=/#/${cfg.captiveDnsAddress}
        local=/#/
      '';
    };
    users.groups.device-core = { };
    users.users.device-core = {
      isSystemUser = true;
      group = "device-core";
      hashedPassword = "!";
    };

    systemd.services.device-core = {
      wantedBy = [ "multi-user.target" ];
      after = [ "systemd-tmpfiles-setup.service" ];
      environment.DEVICE_CORE_DATA_DIR = cfg.dataDir;
      serviceConfig = {
        User = "device-core";
        ExecStart = "${cfg.package}/bin/device-core";
        Restart = "on-failure";
        RestartSec = 2;
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ReadWritePaths = [ cfg.dataDir guard ];
        UMask = "0077";
      };
    };

    systemd.tmpfiles.rules = [
      "d ${cfg.dataDir} 0700 device-core device-core -"
      "d /run/lock/device-core 0755 root root -"
      # Keep the same lock inode across service restarts.
      "f ${guard} 0660 device-core device-core -"
    ];
  };
}
