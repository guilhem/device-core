{ nixpkgs }:

let
  lib = import (nixpkgs + "/lib");
  evaluate = profile:
    (import (nixpkgs + "/nixos/lib/eval-config.nix") {
      system = "x86_64-linux";
      modules = [ ./module.nix { system.stateVersion = "26.05"; } profile ];
    }).config;

  disabled = evaluate { };
  enable = { pkgs, ... }: {
    services.device-core = {
      enable = true;
      package = pkgs.writeShellScriptBin "device-core" "exit 0";
    };
  };
  enabled = evaluate enable;
  captive = evaluate {
    imports = [ enable ];
    services.device-core.captiveDnsAddress = "10.77.0.1";
  };
  captiveFile = "NetworkManager/dnsmasq-shared.d/device-core-captive.conf";
  acceptsCaptiveAddress = address: (builtins.tryEval (evaluate {
    imports = [ enable ];
    services.device-core.captiveDnsAddress = address;
  }).services.device-core.captiveDnsAddress).success;
  extended = evaluate {
    imports = [ enable ];
    services.device-core.dataDir = "/data/device-core";
    users.users.device-core.uid = 1002;
    users.groups.device-core.gid = 1002;
    systemd.services.device-core = {
      after = [ "NetworkManager.service" ];
      unitConfig.ConditionPathExists = [ "!/data/.volatile" ];
      environment.DEVICE_CORE_WIFI_INTERFACE = "wlan0";
    };
  };

  rejectsDataDir = dataDir: !(builtins.tryEval (evaluate {
    imports = [ enable ];
    services.device-core.dataDir = dataDir;
  }).systemd.services.device-core.environment.DEVICE_CORE_DATA_DIR).success;

  service = enabled.systemd.services.device-core;
  local = extended.systemd.services.device-core;
  guard = "/run/lock/device-core/network";
  coreRules = cfg: builtins.filter (lib.hasInfix "device-core") cfg.systemd.tmpfiles.rules;
  busPackages = cfg: builtins.filter (pkg: pkg != cfg.system.path) cfg.services.dbus.packages;
  expectedRules = dataDir: [
    "d ${dataDir} 0700 device-core device-core -"
    "d /run/lock/device-core 0755 root root -"
    "f ${guard} 0660 device-core device-core -"
  ];
in
assert !(disabled.systemd.services ? device-core);
assert !(disabled.users.users ? device-core) && !(disabled.users.groups ? device-core);
assert coreRules disabled == [ ];
assert !(disabled.environment.etc ? ${captiveFile});
assert enabled.services.device-core.captiveDnsAddress == null;
assert !(enabled.environment.etc ? ${captiveFile});
assert !((evaluate { services.device-core.captiveDnsAddress = "10.77.0.1"; }).environment.etc ? ${captiveFile});
assert captive.environment.etc.${captiveFile}.text == ''
  address=/#/10.77.0.1
  local=/#/
'';
assert !captive.networking.networkmanager.enable;
assert !(captive.environment.etc ? "NetworkManager/dnsmasq.d/device-core-captive.conf");
assert builtins.all acceptsCaptiveAddress [
  null "10.77.0.1" "0.0.0.1" "126.255.255.255" "128.0.0.1"
  "223.255.255.255" "240.0.0.1" "255.255.255.254"
];
assert builtins.all (address: !acceptsCaptiveAddress address) [
  "" "0.0.0.0" "127.0.0.1" "127.2.3.4" "255.255.255.255" "224.0.0.1" "239.255.255.255"
  "::1" "::ffff:10.77.0.1" "10.77.0.1:80" "10.77.0.1/24" "example.com"
  "10.077.0.1" "10.77.00.1" "10.77.0.01" "01.2.3.4" "256.0.0.1" "10.77.0" "10.77.0.1.2"
  "-1.2.3.4" "10.77.0.1\n" " 10.77.0.1" "10.77.0.1\nlocal=/#/" 123 false
];

assert enabled.users.users.device-core.isSystemUser;
assert enabled.users.users.device-core.hashedPassword == "!";
assert enabled.users.users.device-core.group == "device-core";
assert enabled.users.users.device-core.uid == null && enabled.users.groups.device-core.gid == null;
assert service.wantedBy == [ "multi-user.target" ];
assert service.after == [ "systemd-tmpfiles-setup.service" ] && service.requires == [ ];
assert removeAttrs service.environment [ "PATH" ] == { DEVICE_CORE_DATA_DIR = "/var/lib/device-core"; };
assert service.serviceConfig == {
  User = "device-core";
  ExecStart = "${enabled.services.device-core.package}/bin/device-core";
  Restart = "on-failure";
  RestartSec = 2;
  NoNewPrivileges = true;
  ProtectSystem = "strict";
  ReadWritePaths = [ "/var/lib/device-core" guard ];
  UMask = "0077";
};
assert coreRules enabled == expectedRules "/var/lib/device-core";
assert busPackages enabled == busPackages disabled;
assert enabled.security.polkit.enable == disabled.security.polkit.enable;
assert enabled.security.polkit.extraConfig == disabled.security.polkit.extraConfig;
assert !enabled.networking.networkmanager.enable;

assert extended.users.users.device-core.uid == 1002 && extended.users.groups.device-core.gid == 1002;
assert builtins.elem "NetworkManager.service" local.after;
assert builtins.elem "systemd-tmpfiles-setup.service" local.after;
assert local.unitConfig.ConditionPathExists == [ "!/data/.volatile" ];
assert removeAttrs local.environment [ "PATH" ] == {
  DEVICE_CORE_DATA_DIR = "/data/device-core";
  DEVICE_CORE_WIFI_INTERFACE = "wlan0";
};
assert local.serviceConfig.ReadWritePaths == [ "/data/device-core" guard ];
assert coreRules extended == expectedRules "/data/device-core";
assert rejectsDataDir "/data/mirror's-state";
assert rejectsDataDir "/";
true
