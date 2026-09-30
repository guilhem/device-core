//! Address the actual RAUC owner. Never infer success from idle or a lost reply.
use crate::options::Options;
use futures_util::StreamExt;
use std::{collections::HashMap, path::Path, time::Duration};
use zbus::{
    message::Type,
    proxy::CacheProperties,
    zvariant::{OwnedValue, Value},
    Connection, MatchRule, MessageStream, Proxy,
};

pub(super) const DEST: &str = "de.pengutronix.rauc";
pub(super) const IFACE: &str = "de.pengutronix.rauc.Installer";
#[derive(Clone, Debug)]
pub(super) struct BootState {
    pub boot_id: String,
    pub slot: String,
    pub health: String,
    pub operation: String,
    pub owner: String,
}
pub(super) enum Outcome {
    Success,
    Refused(String),
    Unknown(String),
}

async fn owner(connection: &Connection) -> Result<String, String> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let bus = Proxy::new(
            connection,
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
        )
        .await?;
        bus.call::<_, _, String>("GetNameOwner", &(DEST,)).await
    })
    .await
    .map_err(|_| "RAUC owner lookup timed out".to_string())?
    .map_err(|e| format!("RAUC owner unknown: {e}"))
}
async fn addressed<'a>(connection: &'a Connection, owner: &'a str) -> zbus::Result<Proxy<'a>> {
    // Properties must come from the current owner, not an old cache.
    zbus::proxy::Builder::new(connection)
        .destination(owner)?
        .path("/")?
        .interface(IFACE)?
        .cache_properties(CacheProperties::No)
        .build()
        .await
}
pub(super) async fn probe(connection: &Connection, options: &Options) -> Result<BootState, String> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let owner = owner(connection).await?;
        let proxy = addressed(connection, &owner)
            .await
            .map_err(|e| e.to_string())?;
        let operation: String = proxy
            .get_property("Operation")
            .await
            .map_err(|e| e.to_string())?;
        let slot: String = proxy
            .get_property("BootSlot")
            .await
            .map_err(|e| e.to_string())?;
        if !matches!(operation.as_str(), "idle" | "installing") || slot.is_empty() {
            return Err("RAUC operation or boot slot unknown".into());
        }
        let boot_id = tokio::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .await
            .map_err(|e| e.to_string())?;
        let marker = tokio::fs::read_to_string(&options.boot_health)
            .await
            .unwrap_or_default();
        if self::owner(connection).await? != owner {
            return Err("RAUC owner changed during boot probe".into());
        }
        Ok(BootState {
            boot_id: boot_id.trim().into(),
            health: marker_health(&marker, &slot),
            slot,
            operation,
            owner,
        })
    })
    .await
    .map_err(|_| "RAUC boot probe timed out".to_string())?
}
pub(super) fn marker_health(marker: &str, slot: &str) -> String {
    let fields: Vec<_> = marker.split_whitespace().collect();
    if fields.len() == 2 && fields[1] == slot && matches!(fields[0], "good" | "stranded") {
        fields[0].into()
    } else {
        String::new()
    }
}

pub(super) async fn install(
    connection: &Connection,
    path: &Path,
    expected_owner: &str,
    progress: impl Fn(i32),
) -> Outcome {
    match observe_install(connection, path, expected_owner, progress).await {
        Ok(outcome) => outcome,
        Err(e) => Outcome::Unknown(e),
    }
}
async fn observe_install(
    connection: &Connection,
    path: &Path,
    expected_owner: &str,
    progress: impl Fn(i32),
) -> Result<Outcome, String> {
    if owner(connection).await? != expected_owner {
        return Err("RAUC owner changed before installation".into());
    }
    let proxy = addressed(connection, expected_owner)
        .await
        .map_err(|e| e.to_string())?;
    let rule = MatchRule::builder()
        .msg_type(Type::Signal)
        .sender(expected_owner)
        .map_err(|e| e.to_string())?
        .path("/")
        .map_err(|e| e.to_string())?
        .interface(IFACE)
        .map_err(|e| e.to_string())?
        .member("Completed")
        .map_err(|e| e.to_string())?
        .build();
    let mut completed = MessageStream::for_match_rule(rule, connection, Some(16))
        .await
        .map_err(|e| e.to_string())?;
    // The idle property reply is a receive-order barrier. Ignore completions from an older install
    // queued after subscribing but before this admission, including a fast Completed before our reply.
    let barrier = connection
        .call_method(
            Some(expected_owner),
            "/",
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &(IFACE, "Operation"),
        )
        .await
        .map_err(|e| e.to_string())?;
    let value: OwnedValue = barrier.body().deserialize().map_err(|e| e.to_string())?;
    let operation = String::try_from(value).map_err(|e| e.to_string())?;
    let after = barrier.recv_position();
    if operation != "idle" {
        return Err("RAUC is busy before InstallBundle".into());
    }
    if owner(connection).await? != expected_owner {
        return Err("RAUC owner changed before InstallBundle".into());
    }
    let path = path.to_str().ok_or("bundle path is not UTF-8")?;
    // No client/request cancellation token is used here. Timeout/lost reply may mean accepted.
    let reply = tokio::time::timeout(
        Duration::from_secs(30),
        proxy.call::<_, _, ()>("InstallBundle", &(path, HashMap::<&str, Value<'_>>::new())),
    )
    .await;
    if let Ok(Err(zbus::Error::MethodError(name, detail, _))) = &reply {
        if !matches!(
            name.as_str(),
            "org.freedesktop.DBus.Error.NoReply"
                | "org.freedesktop.DBus.Error.Disconnected"
                | "org.freedesktop.DBus.Error.ServiceUnknown"
                | "org.freedesktop.DBus.Error.NameHasNoOwner"
                | "org.freedesktop.DBus.Error.Timeout"
        ) {
            return Ok(Outcome::Refused(
                detail.clone().unwrap_or_else(|| name.to_string()),
            ));
        }
    }
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.tick().await;
    loop {
        let message = tokio::select! {
            biased;
            message = completed.next() => {
                message.ok_or("RAUC completion stream disconnected")?.map_err(|e| e.to_string())?
            }
            _ = tick.tick() => {
                if owner(connection).await? != expected_owner { return Err("RAUC owner disappeared or changed during installation".into()); }
                let (percent, _, _): (i32, String, i32) = proxy.get_property("Progress").await.map_err(|e| e.to_string())?;
                progress(percent.clamp(0, 100));
                let operation: String = proxy.get_property("Operation").await.map_err(|e| e.to_string())?;
                if operation == "idle" {
                    // Allow an already queued Completed to be consumed before declaring uncertainty.
                    match tokio::time::timeout(Duration::from_millis(100), completed.next()).await {
                        Ok(Some(Ok(message))) => message,
                        _ => return Err("RAUC is idle but the installation result was lost: retry explicitly".into()),
                    }
                } else {
                    if operation != "installing" { return Err("RAUC operation unknown during installation".into()); }
                    continue;
                }
            }
        };
        if message.recv_position() <= after
            || message.header().sender().map(|s| s.as_str()) != Some(expected_owner)
        {
            continue;
        }
        let code: i32 = message
            .body()
            .deserialize()
            .map_err(|e| format!("invalid RAUC completion: {e}"))?;
        if owner(connection).await? != expected_owner {
            return Err("RAUC owner changed at completion".into());
        }
        if code == 0 {
            return Ok(Outcome::Success);
        }
        let error: String = proxy
            .get_property("LastError")
            .await
            .map_err(|e| e.to_string())?;
        return Ok(Outcome::Refused(if error.is_empty() {
            "RAUC installation failed".into()
        } else {
            error
        }));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn health_is_bound_to_slot() {
        for (marker, want) in [
            ("good A\n", "good"),
            ("stranded A", "stranded"),
            ("good B", ""),
            ("good", ""),
            ("bad A", ""),
            ("", ""),
        ] {
            assert_eq!(super::marker_health(marker, "A"), want);
        }
    }
}
