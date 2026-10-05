use std::{ffi::CString, mem::MaybeUninit, time::Duration};
use zbus::{fdo, message::Header, names::UniqueName, Connection};

/// Resolve a configured account name through NSS, never interpret it as a UID.
pub fn user_uid(user: &str) -> fdo::Result<u32> {
    let name = CString::new(user)
        .ok()
        .filter(|_| !user.is_empty())
        .ok_or_else(|| fdo::Error::AccessDenied("invalid-user".into()))?;
    let mut buffer = vec![0u8; 1024];
    loop {
        let mut entry = MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        // getpwnam_r writes into entry and buffer; no shared passwd storage is used.
        let error = unsafe {
            libc::getpwnam_r(
                name.as_ptr(),
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if error == libc::ERANGE {
            let size = buffer
                .len()
                .checked_mul(2)
                .ok_or_else(|| fdo::Error::AccessDenied("user-unavailable".into()))?;
            buffer.resize(size, 0);
            continue;
        }
        if error != 0 || result.is_null() {
            return Err(fdo::Error::AccessDenied(format!(
                "user-unavailable: {user}"
            )));
        }
        // A successful lookup with a non-null result initialized entry.
        return Ok(unsafe { entry.assume_init() }.pw_uid);
    }
}

/// Only the bus's authenticated Unix UID for this exact connection is authority.
pub async fn sender_uid(connection: &Connection, sender: &str) -> fdo::Result<u32> {
    let lookup = async {
        let sender = UniqueName::try_from(sender)?;
        let bus = fdo::DBusProxy::new(connection).await?;
        Ok::<_, zbus::Error>(bus.get_connection_unix_user(sender.into()).await?)
    };
    tokio::time::timeout(Duration::from_secs(3), lookup)
        .await
        .map_err(|_| fdo::Error::AccessDenied("sender-unavailable".into()))?
        .map_err(|_| fdo::Error::AccessDenied("sender-unavailable".into()))
}

pub async fn authorize_user(
    connection: &Connection,
    header: &Header<'_>,
    expected: &str,
) -> fdo::Result<String> {
    let sender = header
        .sender()
        .ok_or_else(|| fdo::Error::AccessDenied("missing-sender".into()))?
        .to_string();
    if sender_uid(connection, &sender).await? != user_uid(expected)? {
        return Err(fdo::Error::AccessDenied("unauthorized-user".into()));
    }
    Ok(sender)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_users_are_names_and_lookup_fails_closed() {
        assert_eq!(user_uid("root").unwrap(), 0);
        for user in [
            "",
            "root\0other",
            "0",
            "device-core-no-such-account",
            "root:nobody",
        ] {
            assert!(user_uid(user).is_err(), "{user:?}");
        }
    }
}
