use crate::{
    common::{Events, Gate},
    maintenance::Coordinator,
    options::Options,
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use zbus::{fdo, message::Header, object_server::SignalEmitter, zvariant::OwnedObjectPath};

#[derive(Clone)]
pub struct Manager {
    pub coordinator: Coordinator,
    pub events: Events,
    pub gate: Gate,
    pub instance: String,
    pub ready: Arc<AtomicBool>,
    pub options: Options,
}

#[zbus::interface(name = "io.github.guilhem.DeviceCore1.Manager")]
impl Manager {
    #[zbus(property)]
    pub fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").into()
    }
    #[zbus(property)]
    pub fn instance(&self) -> String {
        self.instance.clone()
    }
    #[zbus(property)]
    pub fn ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }
    #[zbus(property)]
    pub fn maintenance(&self) -> bool {
        self.gate.blocked()
    }
    #[zbus(property)]
    pub fn capabilities(&self) -> Vec<String> {
        let mut capabilities = vec![
            "network".into(),
            "audio".into(),
            "config".into(),
            "system".into(),
        ];
        if !self.options.update_repo.is_empty() && !self.options.update_asset.is_empty() {
            capabilities.push("updates".into());
        }
        if !self.options.lva_unit.is_empty() {
            capabilities.push("voice".into());
        }
        if !self.options.maintenance_users.is_empty() {
            capabilities.push("maintenance-agents".into());
        }
        capabilities
    }
    pub async fn register_agent(
        &self,
        path: OwnedObjectPath,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<()> {
        let sender = header
            .sender()
            .ok_or_else(|| fdo::Error::AccessDenied("missing-sender".into()))?;
        self.coordinator.register(sender.as_str(), path).await
    }
    #[zbus(signal)]
    pub async fn event(
        emitter: &SignalEmitter<'_>,
        domain: &str,
        data_json: &str,
    ) -> zbus::Result<()>;
    #[zbus(signal)]
    pub async fn resync(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}
