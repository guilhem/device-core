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
    dataDir = lib.mkOption {
      type = lib.types.externalPath;
      default = "/var/lib/device-core";
      description = "Writable absolute path for persistent device-core state.";
    };
  };

  config = lib.mkIf cfg.enable {
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
