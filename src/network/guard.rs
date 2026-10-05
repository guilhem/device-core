//! Keep this inode stable across daemon restarts. flock locks belong to open
//! descriptions, so an FD handed to a client outlives the daemon that sent it.
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

#[derive(Clone)]
pub struct Guard(PathBuf);

/// Exclusive descriptions are never exported. Unlock explicitly so an
/// incidental fork cannot prolong a completed controller operation until exec.
pub struct Exclusive(File);
impl Drop for Exclusive {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
impl Guard {
    pub fn new(path: PathBuf) -> std::io::Result<Self> {
        let guard = Self(path);
        // The parent unit must provide this writable runtime directory.
        drop(guard.open()?);
        Ok(guard)
    }
    fn open(&self) -> std::io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o660)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&self.0)
    }
    pub fn shared(&self) -> super::nm::Result<File> {
        self.lock(libc::LOCK_SH)
    }
    pub fn exclusive(&self) -> super::nm::Result<Exclusive> {
        self.lock(libc::LOCK_EX).map(Exclusive)
    }
    fn lock(&self, operation: i32) -> super::nm::Result<File> {
        let file = self.open().map_err(|_| "network-guard-unavailable")?;
        if unsafe { libc::flock(file.as_raw_fd(), operation | libc::LOCK_NB) } != 0 {
            return Err(
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock {
                    "network-guard-busy"
                } else {
                    "network-guard-unavailable"
                },
            );
        }
        // Closing the final descriptor releases the lock. Never explicitly
        // LOCK_UN here: a transferred duplicate may still belong to a client.
        Ok(file)
    }
}

#[cfg(test)]
pub(super) async fn wait_for_exclusive(guard: &Guard) -> Exclusive {
    // O_CLOEXEC still permits a concurrent fork to retain a shared description
    // until exec. Wait for its last copy; never unlock a client-owned description.
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match guard.exclusive() {
                Ok(lock) => return lock,
                Err("network-guard-busy") => {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                Err(error) => panic!("{error}"),
            }
        }
    })
    .await
    .expect("shared guard was never released")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn inherited_exclusive_description_cannot_prolong_finished_operation() {
        let path = std::env::temp_dir().join(format!(
            "device-core-exclusive-{}",
            crate::common::token().unwrap()
        ));
        let guard = Guard::new(path.clone()).unwrap();
        let exclusive = guard.exclusive().unwrap();
        let inherited = exclusive.0.try_clone().unwrap();
        assert!(guard.shared().is_err());
        drop(exclusive);
        let shared = guard.shared().unwrap();
        assert!(guard.exclusive().is_err());
        let client = shared.try_clone().unwrap();
        drop(shared);
        assert!(
            guard.exclusive().is_err(),
            "a shared exported duplicate must retain its lock"
        );
        drop(client);
        drop(wait_for_exclusive(&guard).await);
        drop(inherited);
        std::fs::remove_file(path).unwrap();
    }
}
