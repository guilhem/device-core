use super::{Settings, Updater};
use chrono::{DateTime, Datelike, Timelike, Utc};
use std::time::Duration;

pub(super) fn window(settings: &Settings, now: DateTime<Utc>) -> Result<String, String> {
    if settings.start_hour > 23
        || settings.end_hour > 23
        || settings.start_min > 59
        || settings.end_min > 59
    {
        return Err("invalid automatic window".into());
    }
    let tz = settings
        .timezone
        .parse::<chrono_tz::Tz>()
        .map_err(|_| "invalid automatic timezone")?;
    let local = now.with_timezone(&tz);
    let start = settings.start_hour * 60 + settings.start_min;
    let end = settings.end_hour * 60 + settings.end_min;
    let minute = local.hour() * 60 + local.minute();
    if start == end
        || start < end && (minute < start || minute >= end)
        || start > end && minute >= end && minute < start
    {
        return Ok(String::new());
    }
    let mut date = local.date_naive();
    if start > end && minute < end {
        date = date.pred_opt().ok_or("automatic window date underflow")?;
    }
    Ok(format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        date.month(),
        date.day()
    ))
}
pub(super) fn policy(
    settings: &Settings,
    channel: &str,
    now: DateTime<Utc>,
) -> Result<String, String> {
    if !settings.auto_check_updates || !settings.automatic {
        return Err("automatic installation disabled".into());
    }
    if settings.channel != channel {
        return Err("update channel changed".into());
    }
    if !settings.time_reliable {
        return Err("waiting for reliable time".into());
    }
    let window = window(settings, now)?;
    if window.is_empty() {
        return Err("outside automatic window".into());
    }
    Ok(window)
}
impl Updater {
    pub(super) async fn schedule_loop(&self) {
        let mut settings = self.core.settings.clone();
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.tick().await;
        let mut next_check = tokio::time::Instant::now() + Duration::from_secs(300);
        let mut watch_open = true;
        let mut previous = settings.borrow().clone();
        loop {
            let forced = tokio::select! {
                _ = tick.tick() => false,
                _ = self.kick.notified() => true,
                changed = settings.changed(), if watch_open => {
                    if changed.is_err() { watch_open = false; false } else {
                        let s = settings.borrow_and_update().clone();
                        let check = s.auto_check_updates && (!previous.auto_check_updates || previous.channel != s.channel);
                        previous = s;
                        check
                    }
                }
            };
            if self.configured()
                && (forced
                    || settings.borrow().auto_check_updates
                        && tokio::time::Instant::now() >= next_check)
            {
                let _ = self.check().await;
                next_check = tokio::time::Instant::now() + Duration::from_secs(86400);
            }
            if let Err(e) = self.tick(Utc::now()).await {
                crate::debug!("update tick: {e}");
            }
        }
    }
    pub(super) async fn tick(&self, now: DateTime<Utc>) -> Result<(), String> {
        let Ok(guard) = self.core.operation.clone().try_lock_owned() else {
            return Ok(());
        };
        self.core.recover().await?;
        if !self.configured() {
            return Ok(());
        }
        let s = self.core.settings.borrow().clone();
        let status = self.status();
        if self.core.data.lock().unwrap().rebooted || status.suspended || !s.automatic {
            return Ok(());
        }
        if status.state == "reboot" {
            if !status.pending_auto {
                return Ok(());
            }
            policy(&s, &status.pending_channel, now)?;
            let id = if status.operation_id.is_empty() {
                "updates-reboot"
            } else {
                &status.operation_id
            };
            if let Err(e) = self.core.before(&status.pending_channel, true, id).await {
                self.core.recover().await?;
                return Err(e);
            }
            // Reprobe after maintenance callbacks: never reboot over an unknown/running RAUC.
            self.core.power_ready().await?;
            let result = (self.core.hooks.reboot)(id.into()).await;
            if result.is_ok() {
                self.core.data.lock().unwrap().rebooted = true;
            }
            // A failed reboot leaves the system running: recover before releasing maintenance.
            self.core.recover().await?;
            return result;
        }
        if matches!(
            status.state.as_str(),
            "installing" | "downloading" | "confirming" | "uncertain"
        ) || !self.core.checked_recently(now)
        {
            return Ok(());
        }
        let window = policy(&s, &s.channel, now)?;
        if window == status.last_window {
            return Ok(());
        }
        let Some(release) = self
            .releases(&s.channel)
            .into_iter()
            .find(|r| r.ready && r.blocked.is_empty())
        else {
            return Ok(());
        };
        drop(guard);
        // Actor rechecks boot/health/channel/time and acquires maintenance before claiming.
        self.install(&release.tag, &s.channel, true, false).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn midnight_dst_and_policy() {
        let mut s = Settings {
            timezone: "Europe/Paris".into(),
            ..Settings::default()
        };
        for (at, start, end, want) in [
            ("2026-09-29T02:59:00+02:00", 3, 5, ""),
            ("2026-09-29T03:00:00+02:00", 3, 5, "2026-09-29"),
            ("2026-09-29T05:00:00+02:00", 3, 5, ""),
            ("2026-09-30T01:00:00+02:00", 23, 2, "2026-09-29"),
            ("2026-09-30T02:00:00+02:00", 23, 2, ""),
            ("2026-10-25T02:30:00+02:00", 2, 4, "2026-10-25"),
            ("2026-10-25T02:30:00+01:00", 2, 4, "2026-10-25"),
            ("2026-03-29T03:00:00+02:00", 2, 4, "2026-03-29"),
        ] {
            s.start_hour = start;
            s.end_hour = end;
            assert_eq!(
                window(
                    &s,
                    DateTime::parse_from_rfc3339(at)
                        .unwrap()
                        .with_timezone(&Utc)
                )
                .unwrap(),
                want
            );
        }
        let now = DateTime::parse_from_rfc3339("2026-09-29T03:00:00+02:00")
            .unwrap()
            .with_timezone(&Utc);
        s.automatic = true;
        assert!(policy(&s, "stable", now).is_err());
        s.time_reliable = true;
        assert!(policy(&s, "stable", now).is_ok());
        assert!(policy(&s, "test", now).is_err());
        s.auto_check_updates = false;
        assert!(policy(&s, "stable", now).is_err());
    }
}
