# Run device-core on NixOS

Import `nix/module.nix` from a pinned device-core source checkout and supply a
package containing `bin/device-core`. This example builds the package with the
recipe from the same checkout:

```nix
{ pkgs, ... }:

let
  source = ./device-core;
in
{
  imports = [ (source + "/nix/module.nix") ];

  services.device-core = {
    enable = true;
    package = pkgs.callPackage (source + "/nix/package.nix") { src = source; };
  };
}
```

The module starts `device-core.service` under a locked `device-core` system
account and restarts it on failure. State lives in `/var/lib/device-core` by
default. Set `services.device-core.dataDir` to a writable absolute path without
whitespace or systemd specifiers if your image has a separate data partition.
The module also prepares `/run/lock/device-core/network`; keep that path fixed so
network guard users share the same lock across service restarts.

The device profile supplies the system D-Bus policy that lets the daemon own
`io.github.guilhem.DeviceCore1` and grants access to its intended callers.
It also owns any Polkit rules, stable UID/GID assignments, mount and service
ordering, startup conditions, and device specific environment. Configure
NetworkManager, audio tools, or RAUC in that profile if those features are
needed. The module does not enable HTTP.

To check the module without building or booting an image, use the same nixpkgs
checkout as your device configuration:

```sh
nix-instantiate --eval --strict nix/module-test.nix --arg nixpkgs /path/to/nixpkgs
```

A successful evaluation prints `true`.
