use crate::{config::Settings, runtime::App};
use axum::{
    extract::{DefaultBodyLimit, Path, Query, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post, put},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::convert::Infallible;
use zbus::fdo;

struct ApiError(fdo::Error);
impl From<fdo::Error> for ApiError {
    fn from(error: fdo::Error) -> Self {
        Self(error)
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            fdo::Error::InvalidArgs(_) => StatusCode::BAD_REQUEST,
            fdo::Error::AccessDenied(_) => StatusCode::FORBIDDEN,
            fdo::Error::NotSupported(_) => StatusCode::NOT_IMPLEMENTED,
            _ => StatusCode::CONFLICT,
        };
        (status, Json(json!({"error": self.0.to_string()}))).into_response()
    }
}
type Result<T> = std::result::Result<Json<T>, ApiError>;
fn failed(error: String) -> ApiError {
    ApiError(fdo::Error::Failed(error))
}

pub fn router(app: App) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/capabilities", get(capabilities))
        .route("/v1/events", get(events))
        .route("/v1/config", get(config).put(config_update))
        .route("/v1/network", get(network))
        .route("/v1/network/scan", post(network_scan))
        .route(
            "/v1/network/reservation",
            post(network_reserve).delete(network_release),
        )
        .route("/v1/network/authorized", get(network_authorized))
        .route("/v1/network/connect", post(network_connect))
        .route("/v1/network/attempts/{id}/cancel", post(network_cancel))
        .route(
            "/v1/network/profiles/{uuid}",
            axum::routing::delete(network_forget),
        )
        .route("/v1/audio", get(audio))
        .route("/v1/audio/playbacks", post(audio_start))
        .route(
            "/v1/audio/playbacks/{id}",
            get(audio_wait).delete(audio_stop),
        )
        .route("/v1/audio/volume", put(audio_volume))
        .route("/v1/system", get(system))
        .route("/v1/system/time", put(system_time))
        .route("/v1/system/ssh-keys", get(ssh_keys).put(ssh_keys_update))
        .route("/v1/system/reboot", post(system_reboot))
        .route("/v1/system/power-off", post(system_power_off))
        .route("/v1/voice", get(voice))
        .route("/v1/voice/enabled", put(voice_enable))
        .route("/v1/voice/commands", post(voice_command))
        .route("/v1/updates", get(updates))
        .route("/v1/updates/releases", get(releases))
        .route("/v1/updates/check", post(update_check))
        .route("/v1/updates/install", post(update_install))
        .route("/v1/updates/reconcile", post(update_reconcile))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .with_state(app)
}

async fn health(State(app): State<App>) -> Json<Value> {
    Json(
        json!({"ready": app.manager.ready(), "version": app.manager.version(),
        "instance": app.manager.instance(), "maintenance": app.manager.maintenance()}),
    )
}
async fn capabilities(State(app): State<App>) -> Json<Vec<String>> {
    Json(app.manager.capabilities())
}
async fn config(State(app): State<App>) -> Result<Value> {
    let (revision, settings) = app.config.read()?;
    Ok(Json(json!({"revision": revision, "settings": settings})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigUpdate {
    expected_revision: String,
    settings: Settings,
}
async fn config_update(State(app): State<App>, Json(request): Json<ConfigUpdate>) -> Result<Value> {
    Ok(Json(
        json!({"revision": app.config.update(&request.expected_revision, request.settings)?}),
    ))
}
async fn network(State(app): State<App>) -> Json<Value> {
    Json(
        json!({"status": app.network.status(), "networks": app.network.networks(), "profiles": app.network.profiles()}),
    )
}
async fn network_scan(State(app): State<App>) -> Result<Value> {
    app.network.scan()?;
    Ok(Json(json!({})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Token {
    #[serde(default)]
    token: String,
}
async fn network_reserve(State(app): State<App>, Json(request): Json<Token>) -> Result<Value> {
    Ok(Json(json!({"token": app.network.reserve(&request.token)?})))
}
async fn network_release(State(app): State<App>, Json(request): Json<Token>) -> Result<Value> {
    app.network.release(&request.token)?;
    Ok(Json(json!({})))
}
async fn network_authorized(State(app): State<App>, Query(request): Query<Token>) -> Json<Value> {
    Json(json!({"authorized": app.network.authorized(&request.token)}))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Connect {
    ssid: Vec<u8>,
    security: String,
    password: String,
    uuid: String,
    token: String,
}
async fn network_connect(State(app): State<App>, Json(request): Json<Connect>) -> Result<Value> {
    Ok(Json(
        json!({"operation_id": app.network.connect(request.ssid, &request.security, &request.password, &request.uuid, &request.token)?}),
    ))
}
async fn network_cancel(State(app): State<App>, Path(id): Path<u64>) -> Result<Value> {
    app.network.cancel(id)?;
    Ok(Json(json!({})))
}
async fn network_forget(State(app): State<App>, Path(uuid): Path<String>) -> Result<Value> {
    app.network.forget(&uuid).await?;
    Ok(Json(json!({})))
}
async fn audio(State(app): State<App>) -> Json<crate::audio::Status> {
    Json(app.audio.status())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Playback {
    kind: String,
    source: String,
}
async fn audio_start(State(app): State<App>, Json(request): Json<Playback>) -> Result<Value> {
    Ok(Json(
        json!({"operation_id": app.audio.start(&request.kind, &request.source).await?}),
    ))
}
async fn audio_wait(State(app): State<App>, Path(id): Path<String>) -> Result<Value> {
    Ok(Json(
        json!({"id": id, "outcome": app.audio.wait(&id).await?}),
    ))
}
async fn audio_stop(State(app): State<App>, Path(id): Path<String>) -> Result<Value> {
    app.audio.stop(&id).await?;
    Ok(Json(json!({})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Volume {
    percent: u32,
}
async fn audio_volume(State(app): State<App>, Json(request): Json<Volume>) -> Result<Value> {
    app.audio.set_volume(request.percent).await?;
    Ok(Json(json!({})))
}
async fn system(State(app): State<App>) -> Result<Value> {
    let (quality, unix_seconds) = app.system.clock()?;
    Ok(Json(
        json!({"clock": {"quality": quality, "unix_seconds": unix_seconds}, "connectivity": app.system.connectivity().await?}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Time {
    unix_microseconds: i64,
}
async fn system_time(State(app): State<App>, Json(request): Json<Time>) -> Result<Value> {
    app.system.set_time(request.unix_microseconds).await?;
    Ok(Json(json!({})))
}
async fn ssh_keys(State(app): State<App>) -> Result<Value> {
    let (revision, keys) = app.system.get_ssh_keys().await?;
    Ok(Json(json!({"revision": revision, "keys": keys})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Keys {
    expected_revision: String,
    keys: String,
}
async fn ssh_keys_update(State(app): State<App>, Json(request): Json<Keys>) -> Result<Value> {
    let revision = app
        .system
        .set_ssh_keys(&request.expected_revision, &request.keys)
        .await?;
    Ok(Json(json!({"revision": revision})))
}
async fn system_reboot(State(app): State<App>) -> Result<Value> {
    app.system.request_power(false).await?;
    Ok(Json(json!({})))
}
async fn system_power_off(State(app): State<App>) -> Result<Value> {
    app.system.request_power(true).await?;
    Ok(Json(json!({})))
}
async fn voice(State(app): State<App>) -> Json<Value> {
    Json(json!({"supported": app.voice.supported(), "status": app.voice.status()}))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Enabled {
    enabled: bool,
}
async fn voice_enable(State(app): State<App>, Json(request): Json<Enabled>) -> Result<Value> {
    app.voice.enable(request.enabled).await?;
    Ok(Json(json!({})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Command {
    command: String,
}
async fn voice_command(State(app): State<App>, Json(request): Json<Command>) -> Result<Value> {
    app.voice.command(&request.command).await?;
    Ok(Json(json!({})))
}
async fn updates(State(app): State<App>) -> Json<crate::update::Status> {
    Json(app.updates.status())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Channel {
    channel: String,
}
async fn releases(
    State(app): State<App>,
    Query(request): Query<Channel>,
) -> Json<Vec<crate::update::Release>> {
    Json(app.updates.releases(&request.channel))
}
async fn update_check(State(app): State<App>) -> Result<Vec<crate::update::Release>> {
    Ok(Json(app.updates.check().await.map_err(failed)?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Install {
    tag: String,
    channel: String,
    automatic: bool,
    retry: bool,
}
async fn update_install(State(app): State<App>, Json(request): Json<Install>) -> Result<Value> {
    Ok(Json(
        json!({"operation_id": app.updates.install(&request.tag, &request.channel, request.automatic, request.retry).await.map_err(failed)?}),
    ))
}
async fn update_reconcile(State(app): State<App>) -> Result<Value> {
    app.updates.reconcile().await.map_err(failed)?;
    Ok(Json(json!({})))
}
async fn events(
    State(app): State<App>,
) -> Sse<impl futures_util::Stream<Item = std::result::Result<Event, Infallible>>> {
    let mut receiver = app.events.0.subscribe();
    let stream = async_stream::stream! {
        yield Ok(Event::default().event("snapshot").data(json!({
            "instance": app.manager.instance(), "network": app.network.status(),
            "audio": app.audio.status(), "voice": app.voice.status(), "updates": app.updates.status(),
            "maintenance": app.manager.maintenance(),
        }).to_string()));
        loop {
            match receiver.recv().await {
                Ok(event) => yield Ok(Event::default().event(event.domain).data(event.data.to_string())),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    yield Ok(Event::default().event("resync").data("{}"));
                }
                Err(_) => break,
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}
