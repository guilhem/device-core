use std::time::Duration;
use zbus::{fdo, message::Header, zvariant::OwnedObjectPath, Connection, Proxy};

/// Resolve credentials supplied by the bus, never a caller-supplied process ID.
pub async fn sender_unit(connection: &Connection, sender: &str) -> fdo::Result<String> {
    let lookup = async {
        let bus = fdo::DBusProxy::new(connection).await?;
        let credentials = bus.get_connection_credentials(sender.try_into()?).await?;
        let process = credentials
            .process_fd()
            .ok_or_else(|| zbus::Error::Failure("missing ProcessFD".into()))?;
        let manager = Proxy::new(
            connection,
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
        )
        .await?;
        // A pidfd pins the original process; a PID can be reused during lookup.
        let (path, unit_id, _invocation): (OwnedObjectPath, String, Vec<u8>) =
            manager.call("GetUnitByPIDFD", &(process,)).await?;
        let unit = Proxy::new(
            connection,
            "org.freedesktop.systemd1",
            path,
            "org.freedesktop.systemd1.Unit",
        )
        .await?;
        let id: String = unit.get_property("Id").await?;
        if id.is_empty() || id != unit_id {
            return Err(zbus::Error::Failure("unit identity changed".into()));
        }
        Ok::<_, zbus::Error>(id)
    };
    tokio::time::timeout(Duration::from_secs(3), lookup)
        .await
        .map_err(|_| fdo::Error::AccessDenied("sender-unavailable".into()))?
        .map_err(|_| fdo::Error::AccessDenied("sender-unavailable".into()))
}

pub async fn authorize_unit(
    connection: &Connection,
    header: &Header<'_>,
    expected: &str,
) -> fdo::Result<String> {
    let sender = header
        .sender()
        .ok_or_else(|| fdo::Error::AccessDenied("missing-sender".into()))?
        .to_string();
    if expected.is_empty() || sender_unit(connection, &sender).await? != expected {
        return Err(fdo::Error::AccessDenied("unauthorized-unit".into()));
    }
    Ok(sender)
}
