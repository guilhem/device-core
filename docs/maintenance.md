# Maintenance

Manager is exported at `/io/github/guilhem/DeviceCore1`. Its capabilities include
`maintenance-agents` only when the deployment requires product reservations.
Clients always observe Ready and Maintenance; they register callbacks when that
capability is present. Configure `DEVICE_CORE_MAINTENANCE_USERS` with a
colon-separated list of dedicated Unix account names (`Options.maintenance_users`).
NabOS uses `nab-app:nab-hardware`; its presence reporter runs as `nab-hardware`. An unset or
empty list requires no agents. Invalid or unknown accounts, empty list entries
and accounts sharing a UID prevent startup, even in simulation. Two services
sharing a UID cannot be distinguished by account authorization.

`RegisterAgent(path:o)` authenticates the exact caller connection using the bus's
`GetConnectionUnixUser`, then compares that UID with the configured accounts
resolved through libc/NSS. Caller-provided UIDs or PIDs are never authority.
Missing bus credentials and unauthorized accounts deny registration. Registration
pins the unique sender and object path under its configured account name; another
connection of that account replaces its registration. The callback interface is
`io.github.guilhem.DeviceCore1.Agent`:
`Acquire(operation:s)->token:s`, `Release(token:s)` and `Abort(operation:s)`.
Clients authenticate callbacks against the current unique owner of DeviceCore1.
Acquire and Release are idempotent for the same owner and operation/token. Abort
rolls back an acquisition whose reply might have been lost; it cannot release a
different operation. Each client's inactivity barrier covers complete product
sequences, including silent gaps between audio chapters.

The daemon closes audio/voice admission before acquisition, stops the idle voice
unit, then reserves required clients. Acquisition refusal is rolled back by its
operation ID. A failed or missing callback keeps recovery closed. Accepted RAUC
work remains owned by the updater actor and survives the original request.

An agent restart does not prove that RAUC stopped. Reservations track authenticated
accounts and refresh their unique bus owners. After RAUC has conclusively become
idle, a replacement agent acquires the same operation using its own new token
before release. Pending rollbacks also follow the current account registration.
Only after all releases/rollbacks succeed may the coordinator reopen admission.
Clients independently keep their recovery barrier until Manager reports a safe
state. There is no arbitrary reservation expiration.

Power requests through D-Bus and HTTP share the updater's operation lock. They
reconcile the journal, probe the current RAUC owner again after reservation and
refuse a busy or uncertain installation before invoking logind. Unsupported
updates on a fresh independent deployment leave other domains usable.

Host private-bus tests use the current OS account as one authorized identity and
root/nobody as mismatches; successive connections cover restart and stale owner
loss. To validate service isolation in the image, run a private bus permitting the
image's dedicated users and make real calls under each UID. Only `nab-hardware`
may report presence; only `nab-app` and `nab-hardware` may register agents. Keep
`device-core`, `nab-audio` and the operator account denied, require both authorized
agents before acquisition, and verify disconnect/re-registration uses fresh tokens.
This multi-UID check requires root or equivalent user switching; same-UID mock
services cannot establish account isolation.
