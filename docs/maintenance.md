# Maintenance

Manager is exported at `/io/github/guilhem/DeviceCore1`. Its capabilities include
`maintenance-agents` only when the deployment requires product reservations.
Clients always observe Ready and Maintenance; they register callbacks when that
capability is present. NabOS requires both nab-core.service and nab-service.service.

`RegisterAgent(path:o)` authenticates the caller's systemd unit from D-Bus process
credentials, using its `ProcessFD` and systemd `GetUnitByPIDFD` to prevent PID
reuse. Missing process descriptors deny registration. The callback interface is `io.github.guilhem.DeviceCore1.Agent`:
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
units and refresh their unique bus owners. After RAUC has conclusively become
idle, a replacement agent acquires the same operation using its own new token
before release. Pending rollbacks also follow the current unit registration.
Only after all releases/rollbacks succeed may the coordinator reopen admission.
Clients independently keep their recovery barrier until Manager reports a safe
state. There is no arbitrary reservation expiration.

Power requests through D-Bus and HTTP share the updater's operation lock. They
reconcile the journal, probe the current RAUC owner again after reservation and
refuse a busy or uncertain installation before invoking logind. Unsupported
updates on a fresh independent deployment leave other domains usable.
