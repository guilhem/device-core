//! Local descriptors enter the same install actor; only the image helper can prepare trust.
use super::{rauc, Core};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{FileExt, OpenOptionsExt},
    },
    path::Path,
    process::Stdio,
    time::Duration,
};
use tokio::io::AsyncReadExt;

const MAX_BUNDLE: u64 = 2 << 30;

pub(super) fn valid_version(version: &str) -> bool {
    !version.trim().is_empty() && version.len() <= 64 && !version.chars().any(char::is_control)
}

pub(super) fn validate_file(file: &File) -> Result<u64, String> {
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    // SAFETY: fcntl only inspects a live owned descriptor, with no variadic argument.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    if !metadata.is_file()
        || flags & libc::O_ACCMODE == libc::O_WRONLY
        || flags & libc::O_PATH != 0
        || metadata.len() == 0
        || metadata.len() > MAX_BUNDLE
    {
        return Err(
            "bundle descriptor must be a readable nonempty regular file of at most 2 GiB".into(),
        );
    }
    Ok(metadata.len())
}

fn copy(file: File, path: &Path) -> Result<String, String> {
    let expected = validate_file(&file)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| e.to_string())?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 << 10];
    let mut total = 0u64;
    loop {
        // pread keeps the caller's shared seek position intact, including across disconnects.
        let n = file
            .read_at(&mut buffer, total)
            .map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        total = total
            .checked_add(n as u64)
            .ok_or("bundle byte count overflow")?;
        if total > expected || total > MAX_BUNDLE {
            return Err("bundle grew during upload".into());
        }
        output.write_all(&buffer[..n]).map_err(|e| e.to_string())?;
        hash.update(&buffer[..n]);
    }
    if total != expected || file.metadata().map_err(|e| e.to_string())?.len() != expected {
        return Err("bundle size changed during upload".into());
    }
    output.sync_all().map_err(|e| e.to_string())?;
    File::open(path.parent().ok_or("missing update directory")?)
        .and_then(|dir| dir.sync_all())
        .map_err(|e| e.to_string())?;
    Ok(format!("{:x}", hash.finalize()))
}

async fn bundle_version(path: &Path) -> Result<String, String> {
    // Inspection alone does not authorize installation; RAUC verifies the bundle at install time.
    let mut child = tokio::process::Command::new("rauc")
        .args(["info", "--no-verify", "--output-format=json"])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("cannot inspect bundle: {e}"))?;
    let result = tokio::time::timeout(Duration::from_secs(120), async {
        let mut raw = Vec::new();
        child
            .stdout
            .take()
            .ok_or("missing RAUC output")?
            .take((1 << 20) + 1)
            .read_to_end(&mut raw)
            .await
            .map_err(|e| e.to_string())?;
        if raw.len() > 1 << 20 {
            return Err("RAUC manifest output is oversized".into());
        }
        let status = child.wait().await.map_err(|e| e.to_string())?;
        if !status.success() {
            return Err("RAUC could not inspect the bundle manifest".into());
        }
        let manifest: serde_json::Value =
            serde_json::from_slice(&raw).map_err(|e| e.to_string())?;
        let version = manifest
            .get("version")
            .and_then(|v| v.as_str())
            .ok_or("bundle manifest has no version")?;
        if !valid_version(version) {
            return Err(
                "bundle manifest version is empty, too long or contains control characters".into(),
            );
        }
        Ok(version.to_string())
    })
    .await
    .map_err(|_| "bundle inspection timed out".to_string())?;
    if result.is_err() {
        let _ = child.kill().await;
    }
    result
}

impl Core {
    pub(super) fn validate_manual(&self, ignore_certificate: bool) -> Result<(), String> {
        if self.unsupported {
            return Err("RAUC updates are unavailable on this image".into());
        }
        if ignore_certificate
            && (self.options.update_prepare_unit.is_empty()
                || !self.options.update_prepared_bundle.is_absolute()
                || self.options.update_prepared_bundle == self.dir().join("manual.raucb"))
        {
            return Err("certificate bypass requires a preparation unit and a separate absolute prepared bundle path".into());
        }
        Ok(())
    }

    pub(super) async fn install_manual(
        &self,
        file: File,
        ignore_certificate: bool,
        retry: bool,
        id: &str,
    ) -> Result<(), String> {
        self.recover().await?;
        self.admit("", false, retry).await?;
        self.acquire(id).await?;
        let boot = self.admit("", false, retry).await?;
        {
            let mut data = self.data.lock().unwrap();
            data.cleanup_owner = Some(boot.owner);
            data.manual_cleanup = true;
        }
        self.set(|s| s.state = "downloading".into());
        let input = self.dir().join("manual.raucb");
        tokio::fs::create_dir_all(self.dir())
            .await
            .map_err(|e| e.to_string())?;
        let copy_path = input.clone();
        let sum = tokio::task::spawn_blocking(move || copy(file, &copy_path))
            .await
            .map_err(|e| e.to_string())??;
        let tag = bundle_version(&input).await?;
        self.set(|s| s.target = tag.clone());
        self.acquire(id).await?;
        let before = self.admit(&tag, false, retry).await?;
        self.same_owner_idle(&before)?;
        let path = if ignore_certificate {
            // Job acceptance can precede a lost reply; retain cleanup ownership before calling.
            self.data.lock().unwrap().helper_cleanup = true;
            // RemainAfterExit can retain old trust: stop it on proven idle before every preparation.
            crate::system::unit_job_native(
                &self.connection,
                &self.options.update_prepare_unit,
                false,
                Duration::from_secs(120),
            )
            .await
            .map_err(|e| e.to_string())?;
            let after_stop = rauc::probe(&self.connection, &self.options).await?;
            self.same_owner_idle(&after_stop)?;
            crate::system::unit_job_native(
                &self.connection,
                &self.options.update_prepare_unit,
                true,
                Duration::from_secs(1800),
            )
            .await
            .map_err(|e| e.to_string())?;
            let path = &self.options.update_prepared_bundle;
            let prepared = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(path)
                .map_err(|e| format!("cannot read prepared bundle: {e}"))?;
            validate_file(&prepared)?;
            if bundle_version(path).await? != tag {
                return Err("prepared bundle version differs from uploaded manifest".into());
            }
            path.clone()
        } else {
            input
        };
        self.acquire(id).await?;
        let boot = self.admit(&tag, false, retry).await?;
        self.same_owner_idle(&boot)?;
        self.install_prepared(&tag, "stable", false, true, id, &path, &sum, &boot)
            .await
    }

    fn same_owner_idle(&self, boot: &rauc::BootState) -> Result<(), String> {
        let data = self.data.lock().unwrap();
        if boot.operation != "idle"
            || data
                .cleanup_owner
                .as_ref()
                .is_some_and(|owner| owner != &boot.owner)
        {
            self.gate.set(true);
            return Err(
                "RAUC owner changed or is busy: manual bundle retained for reconciliation".into(),
            );
        }
        Ok(())
    }

    /// Called only under the actor lock. Stop orphan preparation before deleting its input.
    pub(super) async fn cleanup_manual(&self, boot: &rauc::BootState) -> Result<(), String> {
        let (input, helper) = {
            let mut data = self.data.lock().unwrap();
            if !data.manual_cleanup && !data.helper_cleanup {
                return Ok(());
            }
            if data
                .cleanup_owner
                .as_ref()
                .is_some_and(|owner| owner != &boot.owner)
            {
                // A new stable owner needs a separate reconciliation before resource release.
                data.cleanup_owner = Some(boot.owner.clone());
                data.recovered = false;
                self.gate.set(true);
                return Err(
                    "RAUC owner changed: reconcile again before cleaning manual resources".into(),
                );
            }
            data.cleanup_owner = Some(boot.owner.clone());
            (data.manual_cleanup, data.helper_cleanup)
        };
        self.same_owner_idle(boot)?;
        if helper {
            crate::system::unit_job_native(
                &self.connection,
                &self.options.update_prepare_unit,
                false,
                Duration::from_secs(120),
            )
            .await
            .map_err(|e| e.to_string())?;
        }
        let after = rauc::probe(&self.connection, &self.options).await?;
        self.same_owner_idle(&after)?;
        if input {
            match tokio::fs::remove_file(self.dir().join("manual.raucb")).await {
                Ok(()) => tokio::fs::File::open(self.dir())
                    .await
                    .map_err(|e| e.to_string())?
                    .sync_all()
                    .await
                    .map_err(|e| e.to_string())?,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        let mut data = self.data.lock().unwrap();
        data.manual_cleanup = false;
        data.helper_cleanup = false;
        data.cleanup_owner = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[test]
    fn descriptors_copy_bounds_and_versions() {
        let dir =
            std::env::temp_dir().join(format!("manual-copy-{}", crate::common::token().unwrap()));
        fs::create_dir(&dir).unwrap();
        let source = dir.join("source");
        fs::write(&source, b"bundle").unwrap();
        let file = File::open(&source).unwrap();
        let destination = dir.join("manual.raucb");
        assert_eq!(
            copy(file, &destination).unwrap(),
            format!("{:x}", Sha256::digest(b"bundle"))
        );
        assert_eq!(fs::read(&destination).unwrap(), b"bundle");
        assert!(copy(File::open(&source).unwrap(), &destination).is_err());
        std::os::unix::fs::symlink(&source, dir.join("symlink")).unwrap();
        assert!(copy(File::open(&source).unwrap(), &dir.join("symlink")).is_err());
        assert!(validate_file(&File::open(&dir).unwrap()).is_err());
        assert!(validate_file(&OpenOptions::new().write(true).open(&source).unwrap()).is_err());
        let file = OpenOptions::new()
            .write(true)
            .read(true)
            .open(&source)
            .unwrap();
        file.set_len(0).unwrap();
        assert!(validate_file(&file).is_err());
        file.set_len(MAX_BUNDLE + 1).unwrap();
        assert!(validate_file(&file).is_err());
        for version in [
            "dev-test",
            "v1.0.0",
            "edge-1.2.3.4",
            "1.0",
            &"x".repeat(64),
            &"é".repeat(32),
        ] {
            assert!(valid_version(version));
        }
        for version in ["", " ", "dev\n", "dev\0", &"x".repeat(65), &"é".repeat(33)] {
            assert!(!valid_version(version));
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
