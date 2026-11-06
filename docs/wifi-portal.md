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
hotspot and open `http://10.77.0.1/` in this example. HTTP GET requests with a
different Host, path, or query on the actual hotspot socket receive a fixed
302 redirect to this canonical URL. Discovery and network scans do not reserve
configuration access. The first page shows saved and discovered networks, manual
entry and a scan button. POST forms use `/` without a query; Host, Origin and
cookies do not authorize or block them. Missing, null, duplicate and foreign
Origins are accepted on the actual hotspot socket. The portal sets no cookies
and has no browser session or Content-Security-Policy restrictions. Forwarded headers
cannot replace the actual socket check.

For offline captive discovery, configure NetworkManager's shared DNS to return
the hotspot address for hostname queries. The Nix module's optional
[`services.device-core.captiveDnsAddress`](nixos.md) supplies this configuration
for all NetworkManager shared connections without changing station DNS. The
portal does not intercept HTTPS or advertise CAPPORT through DHCP option 114.
Automatic setup notifications depend on the phone; the direct HTTP URL remains
available.

Choose a network and select **Connecter l’appareil**. If the network is missing
from the list, use **Saisir un réseau Wi-Fi** to enter its
exact name, security (WPA/WPA2, WPA3 or open) and password. Manual entry also works
when scan results are unavailable; network names must
occupy 1–32 UTF-8 bytes. A single radio serving the recovery hotspot may not be
able to scan nearby networks at the same time.

If the hotspot disappears while the device connects, the attempt can continue after
the browser disconnects. If the device returns to the hotspot, reconnect to it
and reopen the portal to check the outcome.

## Controller authorization and retries

Each submission makes one D-Bus `ConnectHotspot` call with a five-second timeout
detached from browser cancellation. The portal owns no token, mutex or session
and never retries automatically. A later manual click submits a new request;
the controller alone admits concurrent attempts and rejects work while busy.

`ConnectHotspot` supports `open-profile` and `protected-profile` only. Inside the
controller, it synchronously reserves a private token and uses the existing
`Connect` admission path. An admission error releases that exact token. Accepted
work retains it for the actor's final authorization and AP checks, even if the
browser disconnects or the D-Bus reply is lost. The existing Presence mode and
HTTP API retain their authorization requirements.

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
  `org.freedesktop.DBus.Properties.Get` and the `Scan` and `ConnectHotspot`
  methods of `io.github.guilhem.DeviceCore1.Network`
  on destination `io.github.guilhem.DeviceCore1` at
  `/io/github/guilhem/DeviceCore1/Network`. D-Bus policy limits `Get` by path and
  member; the portal uses it for `Status`, `Networks`, and `Profiles`. Leave
  NetworkManager and Polkit privileges with the controller's service account.
  Configure your service supervisor to restart the portal after bus loss.
