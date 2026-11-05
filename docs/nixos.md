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
default. To use a separate data partition, set `services.device-core.dataDir`
to a writable directory such as `"/data/device-core"`. Use a normalized absolute
path outside the Nix store. Each component may contain only ASCII letters,
digits, dots, underscores, and hyphens. The root path `/`, components `.` and
`..`, repeated slashes, and a trailing slash are rejected.

The module also prepares `/run/lock/device-core/network`; keep that path fixed so
network guard users share the same lock across service restarts.

The device profile supplies the system D-Bus policy that lets the daemon own
`io.github.guilhem.DeviceCore1` and grants access to its intended callers.
It also owns any Polkit rules, stable UID/GID assignments, mount and service
ordering, startup conditions, and device specific environment. Configure
NetworkManager, audio tools, or RAUC in that profile if those features are
needed. The module does not enable HTTP.

For offline captive discovery with the optional [Wi-Fi portal](wifi-portal.md),
set `services.device-core.captiveDnsAddress` to the same IPv4 address as the
portal's `--hotspot-address` and `DEVICE_CORE_HOTSPOT_ADDRESS`, for example
`"10.77.0.1"`. The default is `null`, which installs no captive DNS configuration.
The address must be a concrete unicast IPv4 literal without a port, prefix, or
leading-zero octets; loopback, multicast, unspecified and broadcast addresses
are rejected.

When device-core is enabled, this option writes
`/etc/NetworkManager/dnsmasq-shared.d/device-core-captive.conf` with
`address=/#/10.77.0.1` and `local=/#/` for that example. It applies to **all
NetworkManager shared connections**, not station or host DNS. It does not enable
NetworkManager, create a hotspot, start the portal, or change the firewall;
those remain the device profile's responsibility.

To check the module without building or booting an image, use the same nixpkgs
checkout as your device configuration:

```sh
nix-instantiate --eval --strict nix/module-test.nix --arg nixpkgs /path/to/nixpkgs
```

A successful evaluation prints `true`.
