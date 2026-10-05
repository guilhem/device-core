# Network API

Parent integration: `network::start(&Connection, Options, Events).await ->
fdo::Result<Network>` registers the interface and starts its controller on the
supplied connection. The parent alone claims `io.github.guilhem.DeviceCore1`.
No extra dependencies are needed. Declare `pub mod network` in lib.rs.

Path `/io/github/guilhem/DeviceCore1/Network`, interface
`io.github.guilhem.DeviceCore1.Network`. All strings are owned Rust `String`.
Properties and Changed carry native D-Bus structures, not JSON strings.

| Public Rust method / D-Bus | Arguments | Result |
| --- | --- | --- |
| `status()` / Status property | none | Status `(ssbsaystss)` |
| `networks()` / Networks property | none | Vec<NetworkInfo> `a(ayys)` |
| `profiles()` / Profiles property | none | Vec<Profile> `a(say)` |
| `scan()` / Scan | none | `()` |
| `reserve(token: &str)` / Reserve | `s` (empty creates) | reservation `s` |
| `authorized(token: &str)` / Authorized | `s` | `b` |
| `release(token: &str)` / Release | `s` | `()` |
| `connect(ssid: Vec<u8>, security: &str, password: &str, uuid: &str, token: &str)` / Connect | `ayssss` | attempt ID `t` |
| `cancel(attempt_id: u64)` / Cancel | `t` | `()` |
| `forget(uuid: &str).await` / Forget | `s` | `()` |
| `acquire_guard(expected_generation: &str)` / AcquireGuard | `s` | already LOCK_SH locked FD `h` |
| D-Bus only ReportPresence | original kernel CLOCK_MONOTONIC nanoseconds `t` | `()` |

`Status` field order: `mode: String, generation: String, ready: bool,
address: String, ssid: Vec<u8>, profile_uuid: String, attempt_id: u64,
phase: String, error: String`. Mode is unavailable/reconnecting/hotspot/client;
phase idle/connecting/succeeded/cancelled/failed. Generation contains an opaque
random daemon incarnation and a counter. `NetworkInfo`: `ssid: Vec<u8>,
strength: u8, security: String` (open/wpa-psk/sae/unsupported). `Profile`:
`uuid: String, ssid: Vec<u8>`. SSIDs are bytes, never assumed UTF-8. Rust types
implement Serialize, Deserialize and zvariant Type. Go clients mirror each
structure in this exact field order (uint64 for `t`, uint8 for `y`).

Connect with empty UUID creates a candidate; otherwise it activates the native
saved profile without rewriting it. A nonempty token requests physical setup
authorization; its presence lease is consumed. ReportPresence authenticates the
actual unique sender with `auth::authorize_user`, comparing the bus-supplied Unix
UID with the account named by `Options.presence_user` (`DEVICE_CORE_PRESENCE_USER`).
It accepts only original, fresh timestamps after the reservation, and cannot be
called through HTTP. The parent must authenticate against its supplied system
bus (or explicit private simulation bus), never caller-provided UID/PID data. An empty presence user disables reporting;
an unknown account prevents daemon startup. Simulation uses the same authorization.

Read Status, require ready/client, then AcquireGuard(status.generation). The FD
already holds a shared flock: keep it open during download/install. Never unlock
it early. The stable Options.network_guard inode must never be deleted or
replaced on daemon restart; provide its parent directory in the systemd unit.
All controller mutations, including scan and orphan recovery, hold exclusive
flock for the full operation. A surviving client FD blocks a replacement daemon's
mutations. A fresh daemon cannot accept an old incarnation/generation. In HTTP,
use the same Rust methods, but do not expose AcquireGuard as an integer FD or
ReportPresence as an endpoint.

Changed(Status) and standard PropertiesChanged invalidations notify D-Bus.
Events.emit("network", {status, networks, profiles}) feeds the parent SSE bridge.
Simulation changes only in-memory state and the configured temporary guard;
it never invokes host NetworkManager. Real NM calls retain five-second deadlines,
native rollback checkpoints, promotion CAS and ambiguous-result reconciliation.
