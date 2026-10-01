use std::path::PathBuf;

#[derive(Clone)]
pub struct Options {
    pub simulate: bool,
    pub data_dir: PathBuf,
    pub network_guard: PathBuf,
    pub http_addr: Option<String>,
    pub alsa_device: String,
    pub audio_roots: Vec<PathBuf>,
    pub sim_audio_ms: u64,
    pub presence_unit: String,
    pub maintenance_units: Vec<String>,
    pub hotspot_uuid: String,
    pub hotspot_address: String,
    pub hotspot_prefix: String,
    pub lva_unit: String,
    pub lva_url: String,
    pub ssh_unit: String,
    pub ntp_unit: String,
    pub update_repo: String,
    pub update_asset: String,
    pub image_version: String,
    pub update_prepare_unit: String,
    pub update_prepared_bundle: PathBuf,
    pub github_api: String,
    pub github_download: String,
    pub boot_health: PathBuf,
    pub timesync_file: PathBuf,
    pub timesync_clock: PathBuf,
    pub net_probe: String,
}

fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.into())
}
impl Options {
    pub fn from_env(simulate: bool) -> Self {
        let data_dir = PathBuf::from(env("DEVICE_CORE_DATA_DIR", "/var/lib/device-core"));
        Self {
            simulate,
            data_dir,
            network_guard: env("DEVICE_CORE_NETWORK_GUARD", "/run/lock/device-core/network").into(),
            http_addr: std::env::var("DEVICE_CORE_HTTP_ADDR")
                .ok()
                .filter(|v| !v.is_empty()),
            alsa_device: env("DEVICE_CORE_ALSA_DEVICE", "default"),
            audio_roots: env("DEVICE_CORE_AUDIO_ROOTS", "/usr/share/device-core/media")
                .split(':')
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .collect(),
            sim_audio_ms: env("DEVICE_CORE_SIM_AUDIO_MS", "50").parse().unwrap_or(50),
            presence_unit: env("DEVICE_CORE_PRESENCE_UNIT", ""),
            maintenance_units: env("DEVICE_CORE_MAINTENANCE_UNITS", "")
                .split(':')
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect(),
            hotspot_uuid: env(
                "DEVICE_CORE_HOTSPOT_UUID",
                "64657669-6365-4000-8000-000000000001",
            ),
            hotspot_address: env("DEVICE_CORE_HOTSPOT_ADDRESS", "10.41.0.1"),
            hotspot_prefix: env("DEVICE_CORE_HOTSPOT_PREFIX", "Device-"),
            lva_unit: env("DEVICE_CORE_LVA_UNIT", ""),
            lva_url: env("DEVICE_CORE_LVA_URL", "ws://127.0.0.1:6055"),
            ssh_unit: env("DEVICE_CORE_SSH_UNIT", "ssh.service"),
            ntp_unit: env("DEVICE_CORE_NTP_UNIT", "systemd-timesyncd.service"),
            update_repo: env("DEVICE_CORE_UPDATE_REPO", ""),
            update_asset: env("DEVICE_CORE_UPDATE_ASSET", ""),
            image_version: env("DEVICE_CORE_IMAGE_VERSION", "dev"),
            update_prepare_unit: env("DEVICE_CORE_UPDATE_PREPARE_UNIT", ""),
            update_prepared_bundle: env("DEVICE_CORE_UPDATE_PREPARED_BUNDLE", "").into(),
            github_api: env("DEVICE_CORE_GITHUB_API", "https://api.github.com"),
            github_download: env("DEVICE_CORE_GITHUB_DOWNLOAD", "https://github.com"),
            boot_health: env("DEVICE_CORE_BOOT_HEALTH", "/run/device-core-boot-health").into(),
            timesync_file: env(
                "DEVICE_CORE_TIMESYNC_FILE",
                "/run/systemd/timesync/synchronized",
            )
            .into(),
            timesync_clock: env(
                "DEVICE_CORE_TIMESYNC_CLOCK",
                "/var/lib/systemd/timesync/clock",
            )
            .into(),
            net_probe: env("DEVICE_CORE_NET_PROBE", "api.github.com:443"),
        }
    }
}
