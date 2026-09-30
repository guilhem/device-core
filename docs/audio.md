# Audio API

Parent integration: `Audio::new(Options, Gate, Events) -> io::Result<Audio>`;
`audio.serve(&Connection).await -> zbus::Result<()>` registers on the supplied
connection. Declare `pub mod audio` in lib.rs. No additional dependency.
The parent subscribes to org.freedesktop.DBus.NameOwnerChanged and calls
`audio.owner_lost(&str).await` for vanished unique names; retain the server
connection for this subscription.

Path `/io/github/guilhem/DeviceCore1/Audio`, interface
`io.github.guilhem.DeviceCore1.Audio`.

| Rust operation / D-Bus | Arguments | Result |
| --- | --- | --- |
| `start(kind: &str, source: &str).await` / Start | `ss` | playback ID `s` |
| `stop(id: &str).await` / Stop | `s` | `()` |
| `wait(id: &str).await` / Wait | `s` | outcome `s` |
| `status()` / Status property | none | Status `(ssu)` |
| `set_volume(percent: u32).await` / SetVolume | `u` | `()` |

Status field order: `id: String, state: String, volume: u32`; state idle/playing.
All operations return `zbus::fdo::Result<T>` except Status. Structs derive
Serialize/Deserialize/Type. Go clients mirror `(string,string,uint32)`.
Changed(Status) and PropertiesChanged invalidations are emitted by the parent
from Events.emit("audio", Status) through its shared signal bridge.

Kinds are `file` and `stream`. Files must be regular MP3/WAV files whose opened,
canonical path lies under a canonical Options.audio_roots directory; decoding
reads the opened FD so symlink/path replacement cannot change the launched input.
Streams must be literal loopback HTTP URLs (127/8 or ::1), with no credentials
or fragments. The HTTP client disables proxies and redirects and pipes the body
to mpg123 stdin; it never asks mpg123 to fetch URLs/playlists.

Every ID includes a random daemon incarnation. Latest admitted Start preempts the
previous playback after killing/reaping its decoder; concurrent starts serialize.
Stop and Wait target exactly their ID. Outcomes: completed/stopped/preempted/
owner-lost/failed. Unknown IDs fail InvalidArgs("unknown-playback"). The most
recent 256 completed IDs remain queryable; in-progress Wait keeps its receiver.
A stale Stop cannot affect the current playback. D-Bus Start uses the real message
sender and cleans up on disappearance; Rust/HTTP start has no connection owner.
Gate rejects new starts during maintenance, including after waiting for the
previous playback to stop. SetVolume validates 0..100 and applies software gain
using wpctl with a five-second retry budget. Simulation never starts decoders,
network requests or wpctl; it completes after Options.sim_audio_ms.
