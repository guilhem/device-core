use super::{Core, Release};
use futures_util::StreamExt;
use reqwest::{redirect::Policy, StatusCode, Url};
use serde::{de::DeserializeOwned, Deserialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::AsyncWriteExt;

const MAX_BUNDLE: u64 = 2 << 30;
const MAX_SUMS: usize = 64 << 10;
const MAX_JSON: usize = 16 << 20;
const SUMS: &str = "SHA256SUMS";
const STALL: Duration = Duration::from_secs(120);

pub(super) fn version(tag: &str) -> Option<semver::Version> {
    if tag.len() > 64 {
        return None;
    }
    let v = if let Some(edge) = tag.strip_prefix("edge-") {
        let parts: [&str; 4] = edge.split('.').collect::<Vec<_>>().try_into().ok()?;
        if parts.iter().any(|p| {
            p.is_empty()
                || !p.bytes().all(|c| c.is_ascii_digit())
                || p.len() > 1 && p.starts_with('0')
        }) {
            return None;
        }
        // Normalize only for precedence; release identities retain their raw tag.
        semver::Version::parse(&format!(
            "{}.{}.{}-edge.{}",
            parts[0], parts[1], parts[2], parts[3]
        ))
        .ok()?
    } else {
        semver::Version::parse(tag.strip_prefix('v')?).ok()?
    };
    if v.major > 999999 || v.minor > 999999 || v.patch > 999999 {
        return None;
    }
    Some(v)
}
fn name(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
}
pub(super) fn valid_repo(repo: &str) -> bool {
    let parts: Vec<_> = repo.split('/').collect();
    parts.len() == 2 && parts.iter().all(|p| name(p, 100))
}
pub(super) fn valid_asset(asset: &str) -> bool {
    name(asset, 128) && asset != SUMS
}
pub(super) fn valid_hash(sum: &str) -> bool {
    sum.len() == 64
        && sum
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
pub(super) fn allowed(r: &Release, channel: &str) -> bool {
    let edge = r.tag.starts_with("edge-");
    match channel {
        "stable" => !edge && !r.prerelease,
        "test" => !edge,
        "edge" => edge && r.prerelease,
        _ => false,
    }
}
fn loopback(url: &Url) -> bool {
    url.host_str()
        .and_then(|s| s.trim_matches(['[', ']']).parse::<std::net::IpAddr>().ok())
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}
pub(super) fn redirect_allowed(url: &Url, simulate: bool) -> bool {
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    if simulate && loopback(url) && matches!(url.scheme(), "http" | "https") {
        return true;
    }
    url.scheme() == "https"
        && url.port_or_known_default() == Some(443)
        && url
            .host_str()
            .map(|h| {
                ["github.com", "githubusercontent.com"]
                    .iter()
                    .any(|suffix| h == *suffix || h.ends_with(&format!(".{suffix}")))
            })
            .unwrap_or(false)
}
pub(super) fn client(options: &crate::options::Options) -> Result<reqwest::Client, String> {
    for (base, host) in [
        (&options.github_api, "api.github.com"),
        (&options.github_download, "github.com"),
    ] {
        let url = Url::parse(base).map_err(|e| e.to_string())?;
        let simulated =
            options.simulate && loopback(&url) && matches!(url.scheme(), "http" | "https");
        if !simulated
            && (url.scheme() != "https"
                || url.host_str() != Some(host)
                || url.port_or_known_default() != Some(443))
        {
            return Err(format!("untrusted GitHub base {base}"));
        }
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
            || base.ends_with('/')
        {
            return Err(format!("invalid GitHub base {base}"));
        }
    }
    let simulate = options.simulate;
    reqwest::Client::builder()
        .user_agent("device-core-updater")
        .connect_timeout(Duration::from_secs(30))
        .redirect(Policy::custom(move |attempt| {
            if attempt.previous().len() >= 5 {
                return attempt.error("too many redirects");
            }
            if redirect_allowed(attempt.url(), simulate) {
                attempt.follow()
            } else {
                attempt.error("untrusted redirect")
            }
        }))
        .build()
        .map_err(|e| e.to_string())
}

#[derive(Deserialize)]
pub(super) struct GhAsset {
    pub name: String,
    pub size: i64,
    #[serde(default)]
    pub state: String,
    pub browser_download_url: String,
}
#[derive(Deserialize)]
pub(super) struct GhRelease {
    pub tag_name: String,
    pub body: Option<String>,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    pub published_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub assets: Vec<GhAsset>,
}

impl Core {
    fn release(&self, gh: GhRelease) -> Option<Release> {
        let version = version(&gh.tag_name)?;
        if gh.draft {
            return None;
        }
        let mut r = Release {
            tag: gh.tag_name,
            notes: gh.body.unwrap_or_default(),
            published: gh.published_at.map(|t| t.to_rfc3339()).unwrap_or_default(),
            prerelease: gh.prerelease || !version.pre.is_empty(),
            ..Release::default()
        };
        let mut problems = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for a in gh.assets {
            if a.name != self.options.update_asset && a.name != SUMS {
                continue;
            }
            let expected = format!(
                "{}/{}/releases/download/{}/{}",
                self.options.github_download, self.options.update_repo, r.tag, a.name
            );
            let limit = if a.name == SUMS {
                MAX_SUMS as u64
            } else {
                MAX_BUNDLE
            };
            if !seen.insert(a.name.clone()) {
                problems.push(format!("duplicate {} asset", a.name));
            } else if a.state != "uploaded" {
                problems.push(format!("{} is not fully uploaded", a.name));
            } else if a.browser_download_url != expected {
                problems.push(format!("unexpected {} URL", a.name));
            } else if a.size <= 0 || a.size as u64 > limit {
                problems.push(format!("unexpected {} size {}", a.name, a.size));
            } else if a.name == SUMS {
                r.sums_url = a.browser_download_url;
            } else {
                r.bundle_url = a.browser_download_url;
                r.size = a.size as u64;
            }
        }
        if problems.is_empty() && r.bundle_url.is_empty() {
            problems.push(format!("no {} for this board", self.options.update_asset));
        }
        if problems.is_empty() && r.sums_url.is_empty() {
            problems.push(format!("no {SUMS}"));
        }
        r.problem = problems.join("; ");
        r.ready = r.problem.is_empty();
        Some(r)
    }
    async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T, String> {
        let response = self
            .http
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if response.status() != StatusCode::OK {
            return Err(format!("GitHub API: HTTP {}", response.status()));
        }
        let raw = bounded(response, MAX_JSON).await?;
        serde_json::from_slice(&raw).map_err(|e| e.to_string())
    }
    pub(super) async fn fetch_catalog(&self) -> Result<Vec<Release>, String> {
        let mut releases = Vec::new();
        let mut tags = std::collections::HashSet::new();
        // ponytail: catalogue bounded to 1000 releases; stream if the repository outgrows this.
        for page in 1..=10 {
            let list: Vec<GhRelease> = self
                .get_json(&format!(
                    "{}/repos/{}/releases?per_page=100&page={page}",
                    self.options.github_api, self.options.update_repo
                ))
                .await
                .map_err(|e| format!("release catalogue page {page}: {e}"))?;
            let len = list.len();
            if len > 100 {
                return Err("GitHub catalogue page exceeds requested bound".into());
            }
            for gh in list {
                if let Some(r) = self.release(gh) {
                    if !tags.insert(r.tag.clone()) {
                        return Err("duplicate release in catalogue".into());
                    }
                    releases.push(r);
                }
            }
            if len < 100 {
                releases.sort_by(|a, b| {
                    version(&b.tag)
                        .unwrap()
                        .cmp_precedence(&version(&a.tag).unwrap())
                });
                return Ok(releases);
            }
        }
        Err("more than 1000 releases: catalogue incomplete".into())
    }
    pub(super) async fn fetch_release(&self, tag: &str) -> Result<Release, String> {
        tokio::time::timeout(Duration::from_secs(30), async {
            let gh: GhRelease = self
                .get_json(&format!(
                    "{}/repos/{}/releases/tags/{tag}",
                    self.options.github_api, self.options.update_repo
                ))
                .await?;
            let r = self.release(gh).ok_or("release is not published")?;
            if r.tag != tag {
                return Err("GitHub returned a different release tag".into());
            }
            Ok(r)
        })
        .await
        .map_err(|_| "release metadata timed out".to_string())?
    }
    pub(super) async fn checksum(&self, r: &Release) -> Result<String, String> {
        tokio::time::timeout(Duration::from_secs(30), async {
            let response = self
                .http
                .get(&r.sums_url)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            if response.status() != StatusCode::OK {
                return Err(format!("{SUMS}: HTTP {}", response.status()));
            }
            let raw = bounded(response, MAX_SUMS).await?;
            parse_checksum(&raw, &self.options.update_asset)
        })
        .await
        .map_err(|_| "checksums timed out".to_string())?
    }
    pub(super) async fn download(&self, r: &Release, sum: &str) -> Result<PathBuf, String> {
        if !valid_hash(sum) || version(&r.tag).is_none() || r.size == 0 || r.size > MAX_BUNDLE {
            return Err("invalid bundle identity".into());
        }
        let dir = self.dir();
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| e.to_string())?;
        let name = format!("{}-{sum}", r.tag);
        let final_path = dir.join(format!("{name}.raucb"));
        let part = dir.join(format!("{name}.part"));
        let mut entries = tokio::fs::read_dir(&dir).await.map_err(|e| e.to_string())?;
        while let Some(e) = entries.next_entry().await.map_err(|e| e.to_string())? {
            let path = e.path();
            let n = e.file_name();
            let n = n.to_string_lossy();
            if (n.ends_with(".part") || n.ends_with(".raucb")) && path != part && path != final_path
            {
                tokio::fs::remove_file(path)
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
        if verify(final_path.clone(), r.size, sum.into()).await.is_ok() {
            return Ok(final_path);
        }
        remove_if_exists(&final_path).await?;
        let offset = match tokio::fs::symlink_metadata(&part).await {
            Ok(m) if m.is_file() && m.len() <= r.size => m.len(),
            Ok(_) => {
                remove_if_exists(&part).await?;
                0
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e.to_string()),
        };
        if offset < r.size {
            self.fetch(r, &part, offset).await?;
        }
        if let Err(e) = verify(part.clone(), r.size, sum.into()).await {
            remove_if_exists(&part).await?;
            return Err(e);
        }
        tokio::fs::rename(&part, &final_path)
            .await
            .map_err(|e| e.to_string())?;
        tokio::task::spawn_blocking(move || File::open(dir)?.sync_all())
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        Ok(final_path)
    }
    async fn fetch(&self, r: &Release, part: &Path, mut offset: u64) -> Result<(), String> {
        let mut request = self
            .http
            .get(&r.bundle_url)
            .header("Accept-Encoding", "identity");
        if offset > 0 {
            request = request.header("Range", format!("bytes={offset}-"));
        }
        let response = tokio::time::timeout(STALL, request.send())
            .await
            .map_err(|_| "bundle connection stalled".to_string())?
            .map_err(|e| format!("download interrupted (will resume): {e}"))?;
        let append = if offset > 0 && response.status() == StatusCode::PARTIAL_CONTENT {
            let expected = format!("bytes {offset}-{}/{}", r.size - 1, r.size);
            if response
                .headers()
                .get("Content-Range")
                .and_then(|v| v.to_str().ok())
                != Some(expected.as_str())
            {
                remove_if_exists(part).await?;
                return Err("bundle download: unexpected range, will restart".into());
            }
            true
        } else if response.status() == StatusCode::OK {
            offset = 0;
            false
        } else {
            return Err(format!("bundle download: HTTP {}", response.status()));
        };
        if response
            .content_length()
            .is_some_and(|len| len != r.size - offset)
        {
            return Err("bundle Content-Length does not match metadata".into());
        }
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .append(append)
            .truncate(!append)
            .mode(0o640)
            .custom_flags(libc::O_NOFOLLOW)
            .open(part)
            .await
            .map_err(|e| e.to_string())?;
        let mut stream = response.bytes_stream();
        let mut done = offset;
        self.set(|s| s.progress = (done * 100 / r.size) as i32);
        let result = async {
            loop {
                let chunk = tokio::time::timeout(STALL, stream.next())
                    .await
                    .map_err(|_| "download stalled (will resume)".to_string())?;
                let Some(chunk) = chunk else {
                    break;
                };
                let chunk =
                    chunk.map_err(|e| format!("download interrupted (will resume): {e}"))?;
                if chunk.len() as u64 > r.size - done {
                    return Err("bundle exceeds advertised size".into());
                }
                file.write_all(&chunk).await.map_err(|e| e.to_string())?;
                done += chunk.len() as u64;
                let progress = (done * 100 / r.size) as i32;
                if self.data.lock().unwrap().status.progress != progress {
                    self.set(|s| s.progress = progress);
                }
            }
            if done != r.size {
                return Err(format!("bundle size {done}, expected {}", r.size));
            }
            Ok(())
        }
        .await;
        // Also sync resumable prefixes. A rename alone does not make bundle bytes durable.
        file.sync_all().await.map_err(|e| e.to_string())?;
        drop(file);
        if result
            .as_ref()
            .err()
            .is_some_and(|e| e == "bundle exceeds advertised size")
        {
            remove_if_exists(part).await?;
        }
        result
    }
}

async fn bounded(response: reqwest::Response, limit: usize) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|len| len > limit as u64)
    {
        return Err("metadata exceeds size bound".into());
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        if chunk.len() > limit - bytes.len() {
            return Err("metadata exceeds size bound".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
pub(super) fn parse_checksum(raw: &[u8], asset: &str) -> Result<String, String> {
    if raw.len() > MAX_SUMS {
        return Err("checksums exceed size bound".into());
    }
    let text = std::str::from_utf8(raw).map_err(|e| e.to_string())?;
    let mut sum = None;
    for line in text.lines().map(str::trim) {
        if line.len() < 67
            || !line.is_ascii()
            || !valid_hash(&line[..64])
            || &line[64..65] != " "
            || !matches!(&line[65..66], " " | "*")
        {
            continue;
        }
        let file = line[66..].strip_prefix("./").unwrap_or(&line[66..]);
        if !name(file, 128) || file != asset {
            continue;
        }
        let hash = &line[..64];
        if sum.is_some_and(|sum| sum != hash) {
            return Err(format!("conflicting checksums for {asset}"));
        }
        sum = Some(hash);
    }
    sum.map(String::from)
        .ok_or_else(|| format!("{asset} not listed in {SUMS}"))
}
async fn remove_if_exists(path: &Path) -> Result<(), String> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}
async fn verify(path: PathBuf, size: u64, sum: String) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(|e| e.to_string())?;
        let m = file.metadata().map_err(|e| e.to_string())?;
        if !m.is_file() || m.len() != size {
            return Err("bundle size does not match metadata".into());
        }
        let mut hash = Sha256::new();
        let mut buf = [0u8; 64 << 10];
        loop {
            let n = file.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            hash.update(&buf[..n]);
        }
        let got = format!("{:x}", hash.finalize());
        if got != sum {
            return Err("bundle checksum mismatch".into());
        }
        file.sync_all().map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn edge_identity_and_numeric_precedence() {
        for (raw, ordered) in [
            ("edge-0.0.0.0", "0.0.0-edge.0"),
            (
                "edge-999999.999999.999999.99999999999999999999",
                "999999.999999.999999-edge.99999999999999999999",
            ),
        ] {
            assert_eq!(version(raw).unwrap().to_string(), ordered);
        }
        let longest = format!("edge-1.2.3.{}", "9".repeat(53));
        assert_eq!(longest.len(), 64);
        assert!(version(&longest).is_some());
        assert!(version(&format!("{longest}9")).is_none());
        for raw in [
            "edge-1.2.3",
            "edge-1.2.3.4.5",
            "edge-01.2.3.4",
            "edge-1.02.3.4",
            "edge-1.2.03.4",
            "edge-1.2.3.04",
            "edge-1.2.3.",
            "edge-.2.3.4",
            "edge-1.2.3.-4",
            "edge-1.2.3.+4",
            "edge-1.2.3.4+build",
            "edge-1.2.3-rc.4",
            "edge-1.2.3.4\n",
            "edge-1.2.3.４",
            "edge-1000000.2.3.4",
            "edge-1.1000000.3.4",
            "edge-1.2.1000000.4",
            "Edge-1.2.3.4",
            "vedge-1.2.3.4",
        ] {
            assert!(version(raw).is_none(), "{raw:?}");
        }
        for (a, b) in [
            ("edge-1.2.3.10", "edge-1.2.3.2"),
            ("edge-1.10.0.1", "edge-1.9.9.999"),
            (
                "edge-1.2.3.99999999999999999999",
                "edge-1.2.3.9999999999999999999",
            ),
            ("v1.2.3", "edge-1.2.3.10"),
            ("v1.2.3-rc.1", "edge-1.2.3.10"),
            ("edge-1.2.4.0", "v1.2.3"),
        ] {
            assert!(super::super::newer(a, b), "{a} > {b}");
            assert!(!super::super::newer(b, a), "{b} <= {a}");
        }
        for (a, b) in [
            ("edge-1.2.3.10", "edge-1.2.3.10"),
            ("edge-1.2.3.10", "v1.2.3-edge.10"),
            ("v1.2.3-edge.10+build", "edge-1.2.3.10"),
        ] {
            assert!(!super::super::newer(a, b));
            assert!(!super::super::newer(b, a));
        }
    }

    #[test]
    fn strict_identity_checksum_and_redirects() {
        for tag in [
            "v1.4",
            "latest",
            "v01.5.0",
            "v1.5.0-01",
            "v1.5.0;reboot",
            "v1000000.0.0",
        ] {
            assert!(version(tag).is_none());
        }
        for (tag, current) in [
            ("v1.3.0-rc.10", "v1.3.0-rc.2"),
            ("v1.3.0", "v1.3.0-rc.10"),
            ("v1.10.0", "v1.9.9"),
            ("v0.0.1", "dev"),
            ("v1.0.0-99999999999999999999", "v1.0.0-9999999999999999999"),
        ] {
            assert!(super::super::newer(tag, current));
        }
        assert!(!super::super::newer("v1.1.0+rebuild", "v1.1.0"));
        assert!(!valid_repo("../repo"));
        assert!(!valid_repo("owner/repo/extra"));
        assert!(!valid_asset(".."));
        let sum = "a".repeat(64);
        for prefix in ["", "./"] {
            assert_eq!(
                parse_checksum(
                    format!("{sum}  {prefix}bundle.raucb\n").as_bytes(),
                    "bundle.raucb"
                )
                .unwrap(),
                sum
            );
        }
        assert!(parse_checksum(
            format!("{sum}  ../bundle.raucb\n").as_bytes(),
            "bundle.raucb"
        )
        .is_err());
        assert!(parse_checksum(
            format!("{sum}  bundle.raucb\n{}  bundle.raucb\n", "b".repeat(64)).as_bytes(),
            "bundle.raucb"
        )
        .is_err());
        assert!(parse_checksum(&vec![b' '; MAX_SUMS + 1], "bundle.raucb").is_err());
        for url in [
            "http://github.com/b",
            "https://evilgithub.com/b",
            "https://github.com.evil/b",
            "https://github.com:8443/b",
            "http://127.0.0.1/b",
        ] {
            assert!(!redirect_allowed(&Url::parse(url).unwrap(), false));
        }
        assert!(redirect_allowed(
            &Url::parse("https://objects.githubusercontent.com/b").unwrap(),
            false
        ));
        assert!(redirect_allowed(
            &Url::parse("http://127.0.0.1:8080/b").unwrap(),
            true
        ));
    }
}
