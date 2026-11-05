# Wi-Fi setup portal

The optional Wi-Fi setup portal lets someone connected to a device's recovery
hotspot choose a Wi-Fi network from a browser. The interface is in
French. Device-core runs the network controller; the portal is a separate
process and can be omitted from appliances that use another setup flow.

## Build and run

Use the Nix package `import ./wifi-portal { inherit pkgs; }` with your
appliance's Nixpkgs. From the repository root, you can also build it with
`nix-build wifi-portal/default.nix --arg pkgs 'import <nixpkgs> {}'`.
The package installs `bin/device-core-wifi-portal`. For example:

```sh
device-core-wifi-portal --hotspot-address 10.77.0.1
```

Use the actual IPv4 address of your hotspot. The option is required and accepts
a literal address without a port or prefix. The portal listens on that address
on TCP port 80 and waits for the address to become available. Join the recovery
hotspot and open `http://10.77.0.1/` in this example. Use the address directly;
the portal requires an exact Host and, for form submissions, a matching
`http://` Origin. It does not support reverse proxies or forwarded headers.

Select **Configurer le Wi-Fi**, choose a network, and submit the form. If the
hotspot disappears while the device connects, the attempt can continue after
the browser disconnects. If the device returns to the hotspot, reconnect to it
and reopen the portal to check the outcome.

## Appliance prerequisites

- **Recovery hotspot:** For WPA2, provision a system-owned NetworkManager AP
  profile with shared IPv4 and a strong, device-specific PSK, keep it readable
  only by root, and set `DEVICE_CORE_HOTSPOT_AUTH=protected-profile`. For
  passwordless recovery, set `DEVICE_CORE_HOTSPOT_AUTH=open-profile`.
  Device-core creates and saves an open AP if the configured UUID is absent.
  If you provision the open AP yourself, omit the Wi-Fi security section.
  Any nearby client that joins an open hotspot can request setup; HTTP on that
  hotspot does not encrypt the submitted Wi-Fi password. Set
  `DEVICE_CORE_WIFI_INTERFACE` and match `DEVICE_CORE_HOTSPOT_ADDRESS` to the
  portal argument. Use the default profile UUID
  `64657669-6365-4000-8000-000000000001` or set `DEVICE_CORE_HOTSPOT_UUID` to
  the desired UUID. Keep device-core's optional HTTP listener disabled.
  See the [Network API](network.md#recovery-hotspot-profiles).
- **Interface firewall:** Allow inbound TCP port 80 to the configured address
  only on the recovery AP interface. Drop traffic to that address and port on
  every other interface before any broad accept rules. An address bind and HTTP
  checks cannot identify the ingress interface. Do not expose the portal to
  other networks.
- **Service account and system bus:** Run the portal as a dedicated unprivileged
  user with only the permission needed to bind port 80 (for example,
  `CAP_NET_BIND_SERVICE`). On the system bus, allow that user to call only
  `org.freedesktop.DBus.Properties.Get` and the `Scan`, `Reserve`, `Authorized`,
  `Connect`, and `Release` methods of `io.github.guilhem.DeviceCore1.Network`
  on destination `io.github.guilhem.DeviceCore1` at
  `/io/github/guilhem/DeviceCore1/Network`. D-Bus policy limits `Get` by path and
  member; the portal uses it for `Status`, `Networks`, and `Profiles`. Leave
  NetworkManager and Polkit privileges with the controller's service account.
  Configure your service supervisor to restart the portal after bus loss.
