# Config integration

`Store::new(path: impl AsRef<Path>, Events) -> zbus::fdo::Result<Store>`;
path is `options.data_dir.join("settings.json")`. Clone and register Store directly.
`read() -> Result<(String, Settings)>`,
`update(&str, Settings) -> Result<String>`,
`subscribe() -> watch::Receiver<Settings>`.
All writes, including the derived `voice-enabled` flag, belong to Config.
The parent subscribes before reading/applying startup settings, and applies
volume via Audio's runtime method and voice via `Voice::enable_runtime(bool)`.
Config never calls other domains; no callback or nested domain lock.
Audio.SetVolume changes runtime volume only; use Config.Update to persist volume.

Register the handle on the parent's supplied connection at
`/io/github/guilhem/DeviceCore1/Config`, interface
`io.github.guilhem.DeviceCore1.Config`.
`Read() -> (revision:s, settings:(ssub(bs(uu)(uu))b))`;
`Update(expected_revision:s, settings:(ssub(bs(uu)(uu))b)) -> revision:s`.
Settings fields: locale, timezone, volume, auto_check_updates, Updates,
voice_enabled. Updates fields: automatic, channel, start HM, end HM.
HM fields: hour, min. Public Rust/JSON names match these fields exactly.
Channels are `stable` (stable releases), `test` (stable and ordinary prereleases,
excluding Edge), and `edge` (only `edge-*` prereleases).

The new version 1 file is `data_dir/settings.json`; application data and older
NabOS configuration schemas are rejected. Revisions include random incarnation,
counter and SHA256. Defaults are fr_FR, Europe/Paris, volume 100, update checks
on, automatic installation off, stable channel, 03:00–05:00, voice off.
Startup overrides (used only without an existing file):
`DEVICE_CORE_DEFAULT_LOCALE`, `DEVICE_CORE_DEFAULT_TIMEZONE`,
`DEVICE_CORE_DEFAULT_VOLUME`, `DEVICE_CORE_DEFAULT_AUTO_CHECK_UPDATES`,
`DEVICE_CORE_DEFAULT_UPDATES_AUTOMATIC`, `DEVICE_CORE_DEFAULT_UPDATES_CHANNEL`,
`DEVICE_CORE_DEFAULT_UPDATES_START`, `DEVICE_CORE_DEFAULT_UPDATES_END`,
`DEVICE_CORE_DEFAULT_VOICE_ENABLED` (times HH:MM, booleans true/false).

Every replacement fsyncs a mode 0600 temporary file before rename and fsyncs
its directory afterwards. Newly created directories fsync their parent.
settings.json is authoritative; startup reconstructs voice-enabled after a
crash between the two writes. If a post-rename fsync or flag operation fails,
Update returns an error but adopts the visible committed settings and advances
the revision. Read again before retrying. An unchanged Update retries the flag.

The NabOS parent selects `/data/device-core` as the writable system data_dir and
initializes runtime volume to 100 before applying later user changes.
