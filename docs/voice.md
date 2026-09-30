# Voice integration

`Voice::new(Options, Store, System, Events, Gate) -> Voice` is a clone handle.
`supported() -> bool`, `status() -> String`,
`subscribe() -> watch::Receiver<String>`,
`enable(bool)`, `enable_runtime(bool)`, `command(&str)` are public operations
(async operations return `zbus::fdo::Result<()>`).
`enable` persists only through Config; the parent owns its Config watch loop and
calls `enable_runtime` at startup and for each changed voice_enabled value.
Maintenance must close Gate, then await `enable_runtime(false)` before stopping
hardware services; leave Gate closed until recovery, then apply desired config.

Register `/io/github/guilhem/DeviceCore1/Voice`, interface
`io.github.guilhem.DeviceCore1.Voice` on the parent's bus.
Properties: `Supported:b`, `Status:s` (explicit property notifications are
forwarded by the parent from watch). Methods: `Enable(enabled:b)`,
`Command(command:s)`. Signal: `Event(event:s,data_json:s)`.
Forward `Events` domain `voice` objects with event/data fields to that signal;
status updates use event=status with data.status.
The configured lva_unit determines support; empty means unsupported.
The peripheral websocket and its reconnect task own no LEDs or button policy.

Commands are named peripheral commands (1–128 ASCII letters/digits/underscores);
they are encoded as {command: name}, never raw JSON. The queue holds eight
commands and rejects overflow. Enqueue and websocket start_send take short Gate
admissions; no guard crosses an await. Each acknowledgement/write is bounded
to three seconds, connection attempts to five seconds, inbound frames/messages
to 64 KiB. Disconnected commands fail and are never replayed on reconnection.
Enable records acceptance under Gate, then persists outside the admission so
fsync cannot block maintenance closure. An already accepted desired-state write
may finish while Gate is closed; the parent inhibits all runtime application.
LVA event shapes follow the [upstream peripheral API](https://github.com/OHF-Voice/linux-voice-assistant/blob/main/docs/peripheral_api.md).
