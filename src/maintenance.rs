use crate::auth::{sender_uid, user_uid};
use crate::common::{Events, Gate};
use crate::options::Options;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zbus::{fdo, zvariant::OwnedObjectPath, Connection, Proxy};

type Rollback = Option<(String, Vec<String>)>;

#[derive(Clone)]
pub struct Coordinator {
    connection: Connection,
    options: Options,
    agents: Arc<Mutex<HashMap<String, Agent>>>,
    pub gate: Gate,
    events: Events,
    busy: Arc<tokio::sync::Mutex<()>>,
    rollback: Arc<Mutex<Rollback>>,
}

#[derive(Clone)]
struct Agent {
    user: String,
    sender: String,
    path: OwnedObjectPath,
}

pub struct Reservation {
    coordinator: Coordinator,
    operation: String,
    agents: Vec<(Agent, String)>,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl Coordinator {
    pub fn new(connection: Connection, options: Options, gate: Gate, events: Events) -> Self {
        Self {
            connection,
            options,
            gate,
            events,
            agents: Arc::default(),
            busy: Arc::default(),
            rollback: Arc::default(),
        }
    }

    pub async fn register(&self, sender: &str, path: OwnedObjectPath) -> fdo::Result<()> {
        self.options.validate_users()?;
        let uid = sender_uid(&self.connection, sender).await?;
        let mut authorized = None;
        for user in &self.options.maintenance_users {
            if user_uid(user)? == uid {
                authorized = Some(user.clone());
            }
        }
        let user =
            authorized.ok_or_else(|| fdo::Error::AccessDenied("unauthorized-agent".into()))?;
        self.agents.lock().unwrap().insert(
            user.clone(),
            Agent {
                user,
                sender: sender.into(),
                path,
            },
        );
        Ok(())
    }

    pub fn lost(&self, sender: &str) {
        self.agents
            .lock()
            .unwrap()
            .retain(|_, agent| agent.sender != sender);
    }

    fn current(&self, user: &str) -> fdo::Result<Agent> {
        self.agents
            .lock()
            .unwrap()
            .get(user)
            .cloned()
            .ok_or_else(|| fdo::Error::Failed("maintenance-agent-unavailable".into()))
    }

    pub async fn acquire(&self, operation: &str) -> fdo::Result<Reservation> {
        let guard = self
            .busy
            .clone()
            .try_lock_owned()
            .map_err(|_| fdo::Error::Failed("maintenance-busy".into()))?;
        self.abort_pending(operation).await?;
        let agents: Vec<Agent> = {
            let available = self.agents.lock().unwrap();
            self.options
                .maintenance_users
                .iter()
                .map(|user| {
                    available
                        .get(user)
                        .cloned()
                        .ok_or_else(|| fdo::Error::Failed("maintenance-agent-unavailable".into()))
                })
                .collect::<fdo::Result<_>>()?
        };
        self.gate.set(true);
        let mut acquired = Vec::new();
        let mut contacted = Vec::new();
        for agent in agents {
            contacted.push(agent.user.clone());
            let request = async {
                let proxy = Proxy::new(
                    &self.connection,
                    agent.sender.clone(),
                    agent.path.clone(),
                    "io.github.guilhem.DeviceCore1.Agent",
                )
                .await?;
                proxy.call::<_, _, String>("Acquire", &(operation,)).await
            };
            match tokio::time::timeout(Duration::from_secs(5), request).await {
                Ok(Ok(token)) if !token.is_empty() => acquired.push((agent, token)),
                _ => {
                    *self.rollback.lock().unwrap() = Some((operation.into(), contacted));
                    let _ = self.abort_pending(operation).await;
                    return Err(fdo::Error::Failed("maintenance-refused".into()));
                }
            }
        }
        self.events.emit("maintenance", &true);
        Ok(Reservation {
            coordinator: self.clone(),
            operation: operation.into(),
            agents: acquired,
            _guard: guard,
        })
    }

    /// Acquire may have succeeded remotely even when its response was lost.
    pub async fn abort_pending(&self, operation: &str) -> fdo::Result<()> {
        let pending = self.rollback.lock().unwrap().clone();
        let Some((id, agents)) = pending else {
            return Ok(());
        };
        if id != operation {
            return Err(fdo::Error::Failed("maintenance-rollback-pending".into()));
        }
        for user in agents {
            let agent = self.current(&user)?;
            let abort = async {
                let proxy = Proxy::new(
                    &self.connection,
                    agent.sender.clone(),
                    agent.path.clone(),
                    "io.github.guilhem.DeviceCore1.Agent",
                )
                .await?;
                proxy.call::<_, _, ()>("Abort", &(operation,)).await
            };
            tokio::time::timeout(Duration::from_secs(3), abort)
                .await
                .map_err(|_| fdo::Error::Failed("maintenance-abort-timeout".into()))??;
        }
        *self.rollback.lock().unwrap() = None;
        Ok(())
    }

    async fn release_agents(
        &self,
        operation: &str,
        agents: &mut [(Agent, String)],
    ) -> fdo::Result<()> {
        for (agent, token) in agents {
            let current = self.current(&agent.user)?;
            if current.sender != agent.sender || current.path != agent.path {
                // A replacement process owns its own reservation, never the old process's token.
                let acquire = async {
                    let proxy = Proxy::new(
                        &self.connection,
                        current.sender.clone(),
                        current.path.clone(),
                        "io.github.guilhem.DeviceCore1.Agent",
                    )
                    .await?;
                    proxy.call::<_, _, String>("Acquire", &(operation,)).await
                };
                let new_token = tokio::time::timeout(Duration::from_secs(5), acquire)
                    .await
                    .map_err(|_| fdo::Error::Failed("maintenance-reacquire-timeout".into()))??;
                if new_token.is_empty() {
                    return Err(fdo::Error::Failed("maintenance-refused".into()));
                }
                *agent = current;
                *token = new_token;
            }
            let release = async {
                let proxy = Proxy::new(
                    &self.connection,
                    agent.sender.clone(),
                    agent.path.clone(),
                    "io.github.guilhem.DeviceCore1.Agent",
                )
                .await?;
                proxy.call::<_, _, ()>("Release", &(token,)).await
            };
            tokio::time::timeout(Duration::from_secs(3), release)
                .await
                .map_err(|_| fdo::Error::Failed("maintenance-release-timeout".into()))??;
        }
        Ok(())
    }
}

impl Reservation {
    /// Call only after RAUC has a conclusive idle state. A lost client is not
    /// evidence that the installation stopped.
    pub async fn release(&mut self) -> fdo::Result<()> {
        self.coordinator
            .release_agents(&self.operation, &mut self.agents)
            .await?;
        self.coordinator.gate.set(false);
        self.coordinator.events.emit("maintenance", &false);
        Ok(())
    }
}
