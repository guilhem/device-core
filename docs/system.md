# System integration

`System::new(Connection, Options, Events) -> System` uses the parent's bus.
Public async operations: `reboot()`, `power_off()`, `set_time(i64)`,
`get_ssh_keys() -> Result<(String, String)>` (revision, keys),
`set_ssh_keys(expected_revision: &str, keys: &str) -> Result<String>` (new revision),
`connectivity() -> Result<String>`; sync `clock() -> Result<(String, i64)>`.
Results are `zbus::fdo::Result`.
`unit_active(&str) -> Result<bool>` and `unit_job(&str, bool)` are public bounded
runtime primitives for the coordinator and Voice (true=start, false=stop).
`attach_updater(Updater) -> Result<()>` installs the power policy once.
Both transports call async `request_power(off: bool) -> Result<()>`;
raw `reboot()`/`power_off()` are the operations used by coordinator hooks.

Register `/io/github/guilhem/DeviceCore1/System`, interface
`io.github.guilhem.DeviceCore1.System` on the same bus.
`Reboot()`, `PowerOff()`, `SetTime(unix_microseconds:x)`,
`GetSSHKeys()->(revision:s,keys:s)`,
`SetSSHKeys(expected_revision:s,keys:s)->revision:s`,
`Clock()->(quality:s,unix_seconds:x)`, `Connectivity()->s`.
Quality is ntp/manual/restored/unknown; ntp and manual are exact/reliable for
Updater policy, restored is coarse and unknown is unreliable.
Connectivity is ok/lan/offline.
NTP uses StopUnit/StartUnit and waits for job completion; never SetNTP,
EnableUnitFiles, or an /etc write. SSH keys are stored under data_dir/ssh.
Job subscription accepts the existing subscription on the shared parent bus
([systemd Subscribe semantics](https://github.com/systemd/systemd/blob/v257/src/core/dbus-manager.c#L1221-L1249)).
Simulated system operations never address the host's system services or clock.

Bus calls/jobs and operation-lock acquisition are bounded to 15 seconds;
SetTime uses independent NTP resume deadlines even on failure or caller
cancellation. A manual setting is trusted only for the current boot identity.
SSH validation uses the real ssh-keygen on each non-comment line, no shell:
64 KiB total, at most 256 keys, 8 KiB per key line, ten seconds total.
SSH mode is 0600 in a 0700 directory; persistence precedes the runtime job.
GetSSHKeys and the entire CAS/write/runtime job share ssh_lock. System is the
only writer of authorized_keys; the cached tuple is revision plus keys.
Revisions are opaque random tokens plus SHA256 of the exact stored contents.
The initial token is fresh for each System incarnation, and every accepted
write allocates a fresh token before persistence, even for unchanged contents.
This prevents ABA and rejects a stale form or pre-restart token before file or
service mutation. No migration or last-write-wins operation is provided.
Validation completes before the serialized background write/job is accepted.
If a directory fsync fails after rename, System adopts the visible committed
keys and the preallocated revision, then returns the persistence error.
Job failure likewise preserves saved keys and their new revision and returns
an error. Clients must GetSSHKeys again before retrying either failure.
Accepted SSH writes/jobs also finish if the HTTP request disappears.
All stored files are under data_dir;
connectivity probes time out at five seconds. Simulation returns offline and a
private virtual clock, and performs only configured-data-directory file writes.

Read-only-root review covered boot-init/fstab, the image Voice and SSH units and
overrides, and installed ssh/timedated/logind units. The image's timesyncd package
unit was unavailable locally; its [upstream v257 template](https://github.com/systemd/systemd/blob/v257/units/systemd-timesyncd.service.in)
uses RuntimeDirectory and StateDirectory systemd/timesync, the latter bound by
boot-init. Tests use real OpenSSH, local websocket and private D-Bus mocks;
they do not validate the actual package services under the image's effective
systemd restrictions. A mounted read-only root/hardware test remains required;
this host refused the isolated user/mount namespace.
The SSH persistence regression additionally injects a real directory-fsync EIO
in an isolated subprocess using a tiny LD_PRELOAD shim (Linux and cc required).
This exercises a successful rename followed by failed directory fsync, without
adding fault-injection controls to production code.
