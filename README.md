# device-core

Linux device services in Rust, available over the system D-Bus and optional HTTP.
Licensed under GPL-3.0-only. Extracted from NabOS; hardware drivers and product
behaviour belong to the client application.

The daemon runs without MQTT or Nabaztag hardware. NetworkManager, PipeWire,
systemd and optional RAUC/LVA stay behind domain APIs. Product identities,
physical gestures, accounts, animations and application settings belong to clients.

```sh
cargo build --locked
cargo test --locked
```

For a disposable local instance, start a private D-Bus and run:

```sh
export DEVICE_CORE_BUS_ADDRESS="$(dbus-daemon --session --fork --print-address)"
export DEVICE_CORE_DATA_DIR="$(mktemp -d)"
export DEVICE_CORE_NETWORK_GUARD="$DEVICE_CORE_DATA_DIR/network.lock"
export DEVICE_CORE_HTTP_ADDR=127.0.0.1:8081
cargo run --locked -- --simulate
```

Simulation requires an explicit bus address and never invokes the host clock,
power controls, NetworkManager, decoders or LVA. HTTP has **no authentication** and
opens **no listener by default**; production enables it only with an explicit
`DEVICE_CORE_HTTP_ADDR`. Clients use the system D-Bus by default.

The [D-Bus XML](docs/dbus.xml) and [OpenAPI contract](docs/openapi.json) describe
the interfaces. The [domain documentation](docs/network.md) includes network
recovery and physical-presence rules; [audio](docs/audio.md),
[configuration](docs/config.md), [system](docs/system.md),
[voice](docs/voice.md), [updates](docs/updates.md) and
[maintenance](docs/maintenance.md) explain domain behavior.
D-Bus signals and `/v1/events` SSE share events. Re-read snapshots after `Resync`
or an SSE `resync`, and when the Manager's `Instance` changes. Audio IDs include
the daemon incarnation; Stop/Wait never target a later playback.

System settings are stored in `DEVICE_CORE_DATA_DIR/settings.json`, defaulting
to `/var/lib/device-core`; NabOS selects `/data/device-core`. Use Config's revision
for durable compare-and-swap writes. Audio.SetVolume changes runtime gain; use
Config.Update for a persistent desired volume. No application documents or
legacy configuration are imported. Each file has one writer, with file and
directory fsync before a successful reply.

The network lock file must be created under `/run` and **never replaced or
removed during a daemon restart**. AcquireGuard returns a locked file descriptor;
closing all copies releases it. Every NetworkManager mutation acquires an
exclusive lock. ReportPresence accepts original CLOCK_MONOTONIC button timestamps
only from `DEVICE_CORE_PRESENCE_UNIT`, verified using bus credentials and systemd.
Neither operation has an HTTP endpoint.

Updates require the configured repository, image asset and RAUC. A fresh daemon
without update support remains usable. A pending or uncertain journal keeps the
activity gate closed until RAUC recovery is conclusive. Optional LVA requires
`DEVICE_CORE_LVA_UNIT`; an empty value reports unsupported. Maintenance agents
from `DEVICE_CORE_MAINTENANCE_UNITS` reserve product inactivity, and authenticate
the callback sender as the current DeviceCore1 owner. The accepted update actor
survives disconnection of the requesting client.

Linux authorization requires D-Bus `GetConnectionCredentials.ProcessFD` and
systemd `GetUnitByPIDFD` (systemd 255 or later). Missing process descriptors fail
closed; authorization never falls back to recyclable process numbers.

Linux deployment must supply a writable persistent data directory, a stable
runtime lock directory, a working directory under `/run`, the correct PipeWire
session and narrow D-Bus/polkit permissions. Audio playback needs `mpg123`,
`aplay` and `wpctl`; SSH key validation uses `ssh-keygen`. Do not grant raw hardware
capabilities. NabOS owns its systemd/image integration and read-only root mounts.
NabOS image builds archive this repository at an exact commit, its checksum and
Cargo dependencies for offline reconstruction.

Regenerate the XML after changing an interface with
`python3 tools/dbus-contract.py`, after `cargo build`. CI checks it against a
daemon on a private bus. Host tests cover mocks and concurrency; actual device
boot, read-only root operation, audio calibration and RAUC rollback require
hardware validation.
