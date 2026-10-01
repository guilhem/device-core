# Updates integration API

API publication: `update::Updater` is a `Clone` handle. D-Bus and HTTP must use
the same handle; its actor accepts an installation once and owns its lifetime.
There is no request cancellation API. `Options.image_version` is the running
image version (never the Rust package version).

```rust
pub type HookFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;
pub type Hook = Arc<dyn Fn(String) -> HookFuture + Send + Sync>;
pub struct Hooks { pub acquire: Hook, pub release: Hook, pub reboot: Hook }

pub struct Settings {
    pub auto_check_updates: bool,
    pub automatic: bool,
    pub channel: String,
    pub start_hour: u32,
    pub start_min: u32,
    pub end_hour: u32,
    pub end_min: u32,
    pub timezone: String,
    pub time_reliable: bool,
}

impl Updater {
    pub async fn new(connection: zbus::Connection, options: &Options,
        events: Events, gate: Gate, settings: watch::Receiver<Settings>,
        hooks: Hooks) -> Result<Self, String>;
    pub fn configured(&self) -> bool;
    pub fn status(&self) -> Status;
    pub fn releases(&self, channel: &str) -> Vec<Release>;
    pub async fn check(&self) -> Result<Vec<Release>, String>;
    pub async fn install(&self, tag: &str, channel: &str,
        automatic: bool, retry: bool) -> Result<String, String>;
    pub async fn reconcile(&self) -> Result<(), String>;
    pub async fn kick(&self) -> Result<(), String>;
    pub async fn power(&self, action: Hook) -> Result<(), String>;
}
```

The parent maps Config into the flat `Settings` watch value above, updating
`time_reliable` from System clock quality (`ntp` or `manual`; `restored` and
`unknown` are insufficient). Defaults: checks on,
automatic off, stable, 03:00–05:00 UTC, unreliable time. Invalid windows, timezone
or unreliable time prohibit automatic work. The module owns the minute tick,
the initial check after five minutes, daily checks, settings-triggered checks,
window claims and automatic reboot. `kick` requests a catalogue check.

Struct field order below is the D-Bus wire order. Every field is owned;
timestamps are RFC3339 strings (empty means never checked/published).

```rust
pub struct Release {
    pub tag: String,
    pub bundle_url: String,
    pub size: u64,
    pub sums_url: String,
    pub notes: String,
    pub published: String,
    pub prerelease: bool,
    pub ready: bool,
    pub problem: String,
    pub blocked: String,
}
// D-Bus signature: (sstsssbbss); catalogue: a(sstsssbbss)
pub struct Status {
    pub current: String,
    pub state: String,
    pub progress: i32,
    pub error: String,
    pub checked: String,
    pub checking: bool,
    pub check_error: String,
    pub target: String,
    pub last_result: String,
    pub suspended: bool,
    pub retry_required: bool,
    pub pending_auto: bool,
    pub pending_channel: String,
    pub last_window: String,
    pub operation_id: String,
}
// D-Bus signature: (ssissbsssbbbsss)
```

Interface `io.github.guilhem.DeviceCore1.Updates`, object
`/io/github/guilhem/DeviceCore1/Updates`: `Check() -> a(sstsssbbss)`,
`Releases(channel:s) -> a(sstsssbbss)`,
`Install(tag:s, channel:s, automatic:b, retry:b) -> operation_id:s`,
`InstallBundle(fd:h, ignore_certificate:b, retry:b) -> operation_id:s`,
`Reconcile()`, and readable `Status:(ssissbsssbbbsss)` property. Errors use
`org.freedesktop.DBus.Error.Failed`. Status updates also emit `Events` domain
`updates`; the parent forwards events/signals as for other domains. HTTP may
read snapshots synchronously and must return the ID from `install` as accepted
background work. The ID remains in Status through its terminal result.

Each hook receives the operation ID, or `updates-recovery` during startup and
unknown-RAUC recovery. The parent retains its Reservation by that ID; it may
convert fdo errors to strings. `acquire` must be idempotent while held and check the parent's complete
inactivity/maintenance policy, including voice and playback. It runs before
downloading, again before RAUC, and before automatic reboot. `release` is
idempotent and restores the parent coordinator only when safe. `reboot` requests
an actual reboot and returns an error on failure. Hook errors fail/defer work.
Gate closes at construction and before acquisition. No release occurs while
RAUC is busy or its state/owner is unknown, including restart recovery. The
parent must preserve this gate when attaching modules. Only the parent's
release callback reopens the global gate, after all rollback/reservation work
is complete. The module records the ID before calling acquire, including when
acquire fails or times out. It later releases that same ID. RAUC requests and
observation survive the original HTTP/D-Bus caller disconnecting.

With no valid repository/asset configuration, no journal/manual resources, no
preparation unit, and no owned or activatable RAUC service, Status is
`unsupported`: the constructor clears the parent's startup gate before interfaces
are exported. Catalogue configuration is only required for online installs;
a present RAUC service still supports local bundles and recovery. A nonempty
journal still requires recovery even if the image disables update configuration;
unknown RAUC remains fail closed.

System D-Bus/HTTP Reboot and PowerOff must call `Updater::power(Hook)` with the
parent's raw native System operation. This command holds the installation actor's
operation lock through recovery, maintenance acquisition, a fresh RAUC owner/idle
probe and the power hook. Busy/unknown RAUC and an uncertain journal refuse the
request; an installed bundle waiting for reboot permits manual power requests,
including a successful explicit retry whose old suspension has not yet cleared.
Without updater configuration and with an empty journal the power hook is allowed
without probing RAUC. Maintenance still acquires/releases, including rollback
after a failed hook. No separate Updates D-Bus power method is exported. The
existing System signatures remain unchanged. Never pass a hook that reenters
`Updater::power`; use System's raw native operation.

Dependencies required: existing tokio, serde, serde_json, zbus 5, reqwest 0.12
with rustls/stream, sha2, chrono, chrono-tz, semver, futures-util, libc. No new
dependency required. Parent adds `pub mod update` to lib.rs and exports this
handle on its supplied connection. Tests require `dbus-daemon` and use a private
bus plus loopback fake HTTP only under explicit `Options.simulate`.

Persistence is confined to `Options.data_dir/updates` (journal and bundles);
boot ID and `Options.boot_health` are read only. Image service integration must
grant this persistent directory and retain RAUC's inactive-slot permissions.

## Security and recovery

Repository and asset names are restricted to safe ASCII components; tags are
strict `vSemVer`, at most 64 bytes, with core components at most 999999. Build
metadata does not affect precedence. Drafts/invalid tags are excluded, stable
excludes both GitHub-flagged and SemVer prereleases, and the selected tag's
metadata is fetched again before installation. Relevant duplicate assets,
non-uploaded assets and URLs differing from the exact repository/tag/asset path
are rejected. Bundles are limited to 2 GiB, SHA256SUMS to 64 KiB (metadata and
actual body), JSON to 16 MiB per page, catalogue to ten pages of 100 releases.
Any failed/incomplete catalogue check preserves the previous catalogue and
successful-check timestamp. Metadata/checksum fetches time out after 30 seconds;
the complete catalogue check after 60 seconds.

Production bases are GitHub's HTTPS API/download hosts; certificate verification
remains enabled. Redirects stay on HTTPS port 443, only on github.com or
githubusercontent.com and their subdomains, with fewer than five prior requests.
Simulation additionally permits literal loopback IPs; no DNS-based HTTP exception
or insecure TLS switch exists. Bundle streams time out after two minutes without
data. Resume names bind both tag and SHA256. A resumed 206 must match the exact
offset, end and total; 200 restarts from zero. Both cached and new bundles require
exact size and checksum. Partial files and verified bytes are fsynced; final rename
also fsyncs the directory. Wrong hashes/ranges/oversize bodies are removed, while
an interrupted valid prefix remains resumable. RAUC receives only the local
verified path and an empty options dictionary: signature, compatibility and
version-limit checks remain RAUC's responsibility.

The durable journal uses a unique 0600 temporary file, fsync, atomic rename and
directory fsync. It records `installing` before the RAUC call and `installed`
only after an authentic Completed success. RAUC calls and signals are addressed
to its resolved unique owner at `/`; an idle-property reply excludes queued
completions from an older operation. A lost method reply continues observing;
idle without Completed never implies success. Owner changes/disconnection keep
the bundle and recover through the journal, with no release while state is unknown.
The [RAUC Installer API](https://rauc.readthedocs.io/en/latest/reference.html#installer-interface)
defines InstallBundle, Completed and the Operation/Progress properties used here.

After a new boot, target slot, exact image tag and the slot-bound `good` health
marker are all required to confirm success. `stranded`, an interrupted install,
unknown result or unreadable journal suspend automatic updates. Corrupt journals
are archived before replacement; an archive/write failure closes the gate and
fails initialization. A rollback blocks that release until explicit manual retry.
Manual installation can also recover a stranded boot; automatic installation
always requires a healthy released image. Known installed bundles waiting for
reboot cannot be installed again. Retry never overrides a busy/unknown RAUC.

Automatic windows use the configured IANA timezone and local start date across
midnight and repeated DST hours. Start is inclusive and end exclusive. Only
current reliable time, current channel, enabled checks/automatic mode, a fresh
successful catalogue and maintenance admission permit an attempt. The date claim
is fsynced before downloading, at most once per window, and never moves backwards
after a clock correction. Policy, boot/RAUC and maintenance are checked again
before installation and automatic reboot. A successful automatic reboot request
is issued once per daemon incarnation; manual installs leave reboot explicit.

## Local bundles

`InstallBundle` has D-Bus input signature `hbb` and output signature `s`. It
duplicates the received Unix descriptor before acceptance. Only
readable regular files, nonempty and at most 2 GiB, are admitted. The actor owns
the descriptor after the caller disconnects, retains the shared operation lock,
acquires maintenance, copies in 64 KiB chunks to `data_dir/updates/manual.raucb`
with `O_NOFOLLOW`, verifies the byte count, computes SHA256 and fsyncs the file
and directory. It never accepts an input path or URL and never cleans online
resume files during manual uploads.

`rauc info --no-verify --output-format=json` reads the actual manifest version;
inspection does not authorize the installation. Versions must be nonempty,
contain no control characters, and be at most 64 characters. Same versions,
downgrades and development versions are allowed locally. The journal's
`pending.local` field defaults to false: online entries retain strict SemVer
validation, while local entries accept these manifest versions and always have
`automatic=false`. Slot, exact version and health still govern boot reconciliation.
Manual installs never trigger automatic reboot.

Signature verification remains RAUC's responsibility, using an empty install
options dictionary for every install. `ignore_certificate=true` requires
`Options.update_prepare_unit` (`DEVICE_CORE_UPDATE_PREPARE_UNIT`, default empty)
and `Options.update_prepared_bundle` (`DEVICE_CORE_UPDATE_PREPARED_BUNDLE`, default
empty). The latter must be a separate absolute path; on NabOS it is
`/data/nabos-rauc-manual/bundle.raucb`. The image owns preparation, signature integrity
checks, temporary trust and the protected output. The backend waits for native
systemd jobs, stops the helper on confirmed RAUC idle before starting it, and
checks that the prepared manifest version still matches the upload.

The helper is stopped and its input removed only after idle probes from the
same RAUC owner. Unknown/busy RAUC retains resources and maintenance; a changed
owner needs a further reconciliation before cleanup. Startup/recovery stops
orphan preparation before deleting its input or releasing maintenance. The
backend never removes the root-owned prepared output: helper StopUnit owns its
cleanup. With bypass disabled no helper is started.

## Validation

```sh
cargo test --lib update::
cargo test --test update_integration
cargo clippy --all-targets -- -D warnings
```

Six unit checks and twenty integration checks cover the typed wire signatures,
catalogue/security limits, resumed downloads, journal/health/rollback, spoofed and
stale Completed, lost replies/results, client disconnects, unknown-owner recovery,
maintenance rollback, automatic policy and actor-serialized power requests.
Integration checks exercise a private bus and loopback fake HTTP/RAUC/systemd.
Manual checks also require native `rauc`, `openssl` and `mksquashfs` on PATH to
create signed verity bundles and exercise actual manifest inspection.
Actual RAUC signature verification, power cuts, and the complete service with a
read-only root and effective systemd sandbox still require target/image validation.
