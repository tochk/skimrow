//! Self-update: find the latest GitHub release, download and unpack its archive
//! for this platform into a cache directory, and swap it in over the running
//! install. No gpui here: the app (`workspace/update.rs`) runs these blocking
//! calls off the UI thread.
//!
//! Release assets (`.github/workflows/release.yml`) are one zip per target,
//! `skimrow-<os>-<arch>.zip`. Linux / Windows: flat `skimrow[.exe]` +
//! `sqls[.exe]`. macOS: `skimrow.app.zip`, made by `ditto` (which keeps the
//! bundle's symlinks, modes and signature) and unpacked with `ditto` again.
//! Renaming an asset breaks updating for every install that looks for the old
//! name.

use anyhow::{anyhow, bail, Context as _, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub use semver::Version;

/// Overrides the release-metadata URL (a JSON shaped like GitHub's
/// `releases/latest`), to try the whole flow against a local server. The app
/// also lets a development build update itself while it's set.
pub const URL_ENV: &str = "SKIMROW_UPDATE_URL";

/// The app's executable, in an install directory and in an archive.
pub const APP_FILE: &str = if cfg!(windows) {
    "skimrow.exe"
} else {
    "skimrow"
};
/// The completion server shipped next to it.
const SQLS_FILE: &str = if cfg!(windows) { "sqls.exe" } else { "sqls" };
/// What the macOS archive holds, and the bundle unpacking it gives.
#[cfg(target_os = "macos")]
const MAC_ZIP: &str = "skimrow.app.zip";
const MAC_APP: &str = "skimrow.app";

/// Written into a staging directory once it's fully unpacked: the file list.
const COMPLETE: &str = ".complete";

/// A published release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    /// The release's GitHub page (notes, manual download).
    pub page: String,
    /// This platform's archive; `None` when the release has none for it.
    pub asset: Option<Asset>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    /// Bytes; 0 = not given.
    pub size: u64,
    /// Lowercase hex, when GitHub reports a digest (uploads since mid-2025).
    pub sha256: Option<String>,
}

/// This platform's release archive, if releases have one.
pub fn asset_name() -> Option<&'static str> {
    asset_name_for(std::env::consts::OS, std::env::consts::ARCH)
}

pub fn asset_name_for(os: &str, arch: &str) -> Option<&'static str> {
    Some(match (os, arch) {
        ("linux", "x86_64") => "skimrow-linux-x64.zip",
        ("linux", "aarch64") => "skimrow-linux-arm64.zip",
        ("windows", "x86_64") => "skimrow-windows-x64.zip",
        ("windows", "aarch64") => "skimrow-windows-arm64.zip",
        ("macos", "aarch64") => "skimrow-macos-arm64.zip",
        _ => return None,
    })
}

/// Where to ask for the latest release: [`URL_ENV`] if set, else GitHub's API
/// for `repository` (`https://github.com/<owner>/<repo>`).
pub fn endpoint(repository: &str) -> String {
    url_override().unwrap_or_else(|| api_url(repository))
}

/// [`URL_ENV`], when set.
pub fn url_override() -> Option<String> {
    std::env::var(URL_ENV)
        .ok()
        .filter(|url| !url.trim().is_empty())
}

/// GitHub's latest-release endpoint for a repository URL. Stable releases only:
/// it skips drafts and pre-releases.
fn api_url(repository: &str) -> String {
    let path = repository
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .trim_start_matches("https://github.com/");
    format!("https://api.github.com/repos/{path}/releases/latest")
}

// ---- versions -------------------------------------------------------------

/// A development build's version, from `git describe --tags --dirty`:
/// `0.9.0-3-gabc1234`, `0.9.0-dirty`, `0.9.0-3-gabc1234-dirty`.
pub fn is_dev_version(version: &str) -> bool {
    match version.strip_suffix("-dirty") {
        Some(_) => true,
        None => describe_base(version).is_some(),
    }
}

/// `0.9.0-3-gabc1234` → `0.9.0`; `None` without a describe suffix.
fn describe_base(version: &str) -> Option<&str> {
    let (head, hash) = version.rsplit_once('-')?;
    let (base, count) = head.rsplit_once('-')?;
    let hash = hash.strip_prefix('g')?;
    let hex = hash.len() >= 4 && hash.chars().all(|c| c.is_ascii_hexdigit());
    let count_ok = !count.is_empty() && count.chars().all(|c| c.is_ascii_digit());
    (hex && count_ok).then_some(base)
}

/// The release a version string stands for. A development build counts as the
/// tag it was described from: semver would read `0.9.0-3-gabc1234` as a
/// PRE-release of 0.9.0, i.e. older than the build it came after.
pub fn base_version(version: &str) -> Option<Version> {
    let version = version.strip_suffix("-dirty").unwrap_or(version);
    let version = describe_base(version).unwrap_or(version);
    Version::parse(version.trim_start_matches('v')).ok()
}

/// `latest` is newer than the running `current` (an unreadable `current` never
/// is: nothing to compare with).
pub fn is_newer(current: &str, latest: &Version) -> bool {
    base_version(current).is_some_and(|current| *latest > current)
}

// ---- release metadata -----------------------------------------------------

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    digest: Option<String>,
}

/// A `releases/latest` answer, with `asset` (this platform's archive) picked.
pub fn parse_release(json: &str, asset: Option<&str>) -> Result<Release> {
    let gh: GhRelease = serde_json::from_str(json).context("unexpected release metadata")?;
    let version = Version::parse(gh.tag_name.trim_start_matches('v'))
        .with_context(|| format!("release tag {:?} is not a version", gh.tag_name))?;
    let asset = asset
        .and_then(|want| gh.assets.into_iter().find(|a| a.name == want))
        .map(|a| Asset {
            sha256: a.digest.as_deref().and_then(sha256_of_digest),
            name: a.name,
            url: a.browser_download_url,
            size: a.size,
        });
    Ok(Release {
        version,
        page: gh.html_url,
        asset,
    })
}

/// `sha256:<64 hex>` → the hex; any other algorithm / shape → `None`.
fn sha256_of_digest(digest: &str) -> Option<String> {
    let hex = digest.strip_prefix("sha256:")?.to_ascii_lowercase();
    (hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit())).then_some(hex)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---- network ----------------------------------------------------------------

/// GitHub's certificate is checked against the OS trust store (corporate CAs
/// work), loaded once; Mozilla's roots if that comes up empty. `HTTPS_PROXY`
/// & co. are honored (ureq reads them by default).
fn agent(user_agent: &str) -> ureq::Agent {
    use ureq::tls::TlsConfig;
    ureq::Agent::config_builder()
        .user_agent(user_agent)
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_per_read(Some(Duration::from_secs(30)))
        .tls_config(TlsConfig::builder().root_certs(roots()).build())
        .build()
        .new_agent()
}

fn roots() -> ureq::tls::RootCerts {
    use ureq::tls::{Certificate, RootCerts};
    static ROOTS: std::sync::OnceLock<RootCerts> = std::sync::OnceLock::new();
    ROOTS
        .get_or_init(|| {
            let native = rustls_native_certs::load_native_certs();
            if native.certs.is_empty() {
                return RootCerts::WebPki;
            }
            let certs: Vec<Certificate<'static>> = native
                .certs
                .iter()
                .map(|der| Certificate::from_der(der.as_ref()).to_owned())
                .collect();
            RootCerts::Specific(std::sync::Arc::new(certs))
        })
        .clone()
}

fn http_error(e: ureq::Error) -> anyhow::Error {
    match e {
        ureq::Error::StatusCode(403 | 429) => {
            anyhow!("GitHub refused the request (rate limit?); try again later")
        }
        ureq::Error::StatusCode(404) => anyhow!("no release found"),
        ureq::Error::StatusCode(code) => anyhow!("the server answered HTTP {code}"),
        e => anyhow!(e),
    }
}

/// Ask `url` for the latest release. `user_agent` is required by GitHub; keep
/// it generic (`skimrow/<version>`).
pub fn fetch_latest(url: &str, user_agent: &str, asset: Option<&str>) -> Result<Release> {
    let mut response = agent(user_agent)
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(http_error)?;
    let json = response.body_mut().read_to_string().map_err(http_error)?;
    parse_release(&json, asset)
}

/// Download `asset` into `dir`, counting bytes into `progress`. The size and
/// digest are checked when the release gave them; a failed download leaves
/// nothing behind.
pub fn download(
    asset: &Asset,
    dir: &Path,
    user_agent: &str,
    progress: &AtomicU64,
) -> Result<PathBuf> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let part = dir.join(format!("{}.part", asset.name));
    let result = fetch_to(asset, &part, user_agent, progress);
    if result.is_err() {
        let _ = fs::remove_file(&part);
    }
    result?;
    let done = dir.join(&asset.name);
    fs::rename(&part, &done).with_context(|| format!("saving {}", done.display()))?;
    Ok(done)
}

fn fetch_to(asset: &Asset, path: &Path, user_agent: &str, progress: &AtomicU64) -> Result<()> {
    let response = agent(user_agent)
        .get(&asset.url)
        .call()
        .map_err(http_error)?;
    // The size is known for a GitHub asset; a body past it is not the asset.
    let limit = if asset.size > 0 { asset.size } else { 1 << 30 };
    let mut body = response
        .into_body()
        .into_with_config()
        .limit(limit)
        .reader();
    let mut file =
        fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buf = vec![0u8; 64 * 1024];
    progress.store(0, Ordering::Relaxed);
    loop {
        let n = body.read(&mut buf).context("download interrupted")?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .with_context(|| format!("writing {}", path.display()))?;
        hasher.update(&buf[..n]);
        total += n as u64;
        progress.store(total, Ordering::Relaxed);
    }
    file.sync_all()?;
    if asset.size > 0 && total != asset.size {
        bail!("download incomplete ({total} of {} bytes)", asset.size);
    }
    if let Some(want) = &asset.sha256 {
        if hex(&hasher.finalize()) != *want {
            bail!("download corrupted (its SHA-256 doesn't match the release's)");
        }
    }
    Ok(())
}

// ---- staging ----------------------------------------------------------------

/// An unpacked update, ready to install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Staged {
    pub version: Version,
    /// Holds the unpacked files.
    pub dir: PathBuf,
    /// Their names, the app's (`skimrow`, `skimrow.exe`, `skimrow.app`) first.
    pub files: Vec<String>,
}

impl Staged {
    /// A finished staging directory from before.
    fn load(dir: &Path, version: &Version) -> Option<Self> {
        let list = fs::read_to_string(dir.join(COMPLETE)).ok()?;
        let files: Vec<String> = list
            .lines()
            .filter(|f| !f.is_empty())
            .map(str::to_string)
            .collect();
        let whole = !files.is_empty() && files.iter().all(|f| dir.join(f).exists());
        whole.then(|| Staged {
            version: version.clone(),
            dir: dir.to_path_buf(),
            files,
        })
    }
}

/// Get `release` ready to install under `cache` (a directory of its own, as
/// `<cache>/<version>/`): reuse a finished earlier download of that version,
/// else download and unpack it. Other versions' directories are removed.
pub fn prepare(
    release: &Release,
    cache: &Path,
    user_agent: &str,
    progress: &AtomicU64,
) -> Result<Staged> {
    let asset = release
        .asset
        .as_ref()
        .ok_or_else(|| anyhow!("the release has no download for this platform"))?;
    let root = cache.join(release.version.to_string());
    let files = root.join("files");
    if let Some(staged) = Staged::load(&files, &release.version) {
        return Ok(staged);
    }
    if let Ok(entries) = fs::read_dir(cache) {
        for entry in entries.flatten() {
            if entry.path() != root {
                remove_path(&entry.path());
            }
        }
    }
    remove_path(&root);
    let zip = download(asset, &root, user_agent, progress)?;
    let names = unpack(&zip, &files)?;
    fs::write(files.join(COMPLETE), names.join("\n"))?;
    let _ = fs::remove_file(&zip);
    Ok(Staged {
        version: release.version.clone(),
        dir: files,
        files: names,
    })
}

/// Drop staged versions in `cache` that aren't newer than the running
/// `current` (installed by now, or older), and anything else that isn't one.
pub fn prune(cache: &Path, current: &str) {
    let Ok(entries) = fs::read_dir(cache) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let newer = name
            .to_str()
            .and_then(|n| Version::parse(n).ok())
            .is_some_and(|v| is_newer(current, &v));
        if !newer {
            remove_path(&entry.path());
        }
    }
}

/// Unpack an update archive into `dir` (emptied first). Takes only the names
/// an update consists of, at the archive's top level — nothing else, so no
/// entry can land outside `dir`. Returns the files, the app's first.
pub fn unpack(zip: &Path, dir: &Path) -> Result<Vec<String>> {
    remove_path(dir);
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let file = fs::File::open(zip).with_context(|| format!("opening {}", zip.display()))?;
    let mut archive = zip::ZipArchive::new(file).context("the download is not a zip archive")?;
    #[cfg(target_os = "macos")]
    let wanted: &[&str] = &[MAC_ZIP];
    #[cfg(not(target_os = "macos"))]
    let wanted: &[&str] = &[APP_FILE, SQLS_FILE];
    let mut names = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).context("damaged archive")?;
        if entry.is_dir() {
            continue;
        }
        let entry_name = entry.name().context("damaged archive")?.into_owned();
        let Some(&name) = wanted.iter().find(|w| **w == entry_name) else {
            continue;
        };
        let path = dir.join(name);
        let mut out =
            fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
        std::io::copy(&mut entry, &mut out).context("damaged archive")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
        }
        names.push(name.to_string());
    }
    #[cfg(target_os = "macos")]
    let mut names = unpack_bundle(dir, names)?;
    let app = if cfg!(target_os = "macos") {
        MAC_APP
    } else {
        APP_FILE
    };
    if !names.iter().any(|n| n == app) {
        bail!("the download has no {app}");
    }
    names.sort_by_key(|n| n != app);
    Ok(names)
}

/// macOS: the archive held `skimrow.app.zip`; `ditto` unpacks it into the
/// bundle (keeping its symlinks, modes and code signature).
#[cfg(target_os = "macos")]
fn unpack_bundle(dir: &Path, names: Vec<String>) -> Result<Vec<String>> {
    if !names.iter().any(|n| n == MAC_ZIP) {
        return Ok(names);
    }
    let zip = dir.join(MAC_ZIP);
    let status = std::process::Command::new("/usr/bin/ditto")
        .arg("-x")
        .arg("-k")
        .arg(&zip)
        .arg(dir)
        .status()
        .context("running ditto")?;
    if !status.success() {
        bail!("ditto could not unpack {MAC_ZIP} ({status})");
    }
    let _ = fs::remove_file(&zip);
    if !dir.join(MAC_APP).join("Contents/MacOS/skimrow").is_file() {
        bail!("{MAC_ZIP} holds no skimrow.app");
    }
    Ok(vec![MAC_APP.to_string()])
}

// ---- installing -------------------------------------------------------------

/// Where the running app is installed. Find it at STARTUP: on Linux
/// `current_exe` reads `/proc/self/exe`, which turns into `… (deleted)` once
/// the binary has been replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallTarget {
    /// The executable (Linux / Windows) or bundle (macOS): what's replaced, and
    /// what to relaunch.
    pub app: PathBuf,
    /// Its directory, where the other files (sqls) go.
    pub dir: PathBuf,
}

impl InstallTarget {
    /// The running app's.
    pub fn current() -> Result<Self> {
        Self::locate(&std::env::current_exe().context("can't tell where skimrow runs from")?)
    }

    /// The install `exe` belongs to (on macOS its enclosing `.app`).
    pub fn locate(exe: &Path) -> Result<Self> {
        let app = if cfg!(target_os = "macos") {
            bundle_of(exe).ok_or_else(|| anyhow!("skimrow isn't running from an app bundle"))?
        } else {
            exe.to_path_buf()
        };
        let dir = app
            .parent()
            .ok_or_else(|| anyhow!("{} has no parent directory", app.display()))?
            .to_path_buf();
        Ok(Self { app, dir })
    }

    /// The install can be replaced by this user (else: download it by hand, or
    /// update it the way it was installed).
    pub fn check_writable(&self) -> Result<()> {
        let probe = self
            .dir
            .join(format!(".skimrow-update-{}", std::process::id()));
        fs::File::create(&probe)
            .and_then(|_| fs::remove_file(&probe))
            .map_err(|e| anyhow!("can't write to {}: {e}", self.dir.display()))
    }

    /// Where a staged file goes.
    fn target_of(&self, staged: &str) -> PathBuf {
        if staged == APP_FILE || staged == MAC_APP {
            self.app.clone()
        } else {
            self.dir.join(staged)
        }
    }
}

/// `<bundle>.app/Contents/MacOS/<exe>` → the bundle.
fn bundle_of(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    let shaped = macos.file_name()? == "MacOS" && contents.file_name()? == "Contents";
    (shaped && bundle.extension()? == "app").then(|| bundle.to_path_buf())
}

/// Put `staged` in place of the install. Every file is first copied next to
/// its target (`<name>.new`: same filesystem, so the swap is renames only);
/// then the target moves aside to `<name>.old` — a running executable may be
/// renamed even on Windows, just not deleted or overwritten — and the new one
/// moves in. A failure puts back whatever was already swapped. The `.old`
/// copies are deleted when the OS lets us (not on Windows while they run);
/// [`cleanup`] at the next start gets the rest.
pub fn install(staged: &Staged, target: &InstallTarget) -> Result<()> {
    let pairs: Vec<(PathBuf, PathBuf)> = staged
        .files
        .iter()
        .map(|f| (staged.dir.join(f), target.target_of(f)))
        .collect();
    for (src, dst) in &pairs {
        let new = sibling(dst, "new");
        remove_path(&new);
        if let Err(e) = copy_path(src, &new) {
            for (_, dst) in &pairs {
                remove_path(&sibling(dst, "new"));
            }
            return Err(e.context(format!("copying the update to {}", new.display())));
        }
    }
    // (target, whether it existed before) for each one swapped so far.
    let mut swapped: Vec<(&Path, bool)> = Vec::new();
    for (_, dst) in &pairs {
        match swap(dst) {
            Ok(existed) => swapped.push((dst, existed)),
            Err(e) => {
                for (dst, existed) in swapped.into_iter().rev() {
                    remove_path(dst);
                    if existed {
                        let _ = rename_retry(&sibling(dst, "old"), dst);
                    }
                }
                for (_, dst) in &pairs {
                    remove_path(&sibling(dst, "new"));
                }
                return Err(e.context(format!("replacing {}", dst.display())));
            }
        }
    }
    for (_, dst) in &pairs {
        remove_path(&sibling(dst, "old"));
    }
    Ok(())
}

/// `dst` → `dst.old`, `dst.new` → `dst`. Returns whether `dst` existed.
fn swap(dst: &Path) -> Result<bool> {
    let new = sibling(dst, "new");
    let old = sibling(dst, "old");
    let existed = dst.symlink_metadata().is_ok();
    if existed {
        remove_path(&old);
        rename_retry(dst, &old)?;
    }
    if let Err(e) = rename_retry(&new, dst) {
        if existed {
            let _ = rename_retry(&old, dst);
        }
        return Err(e);
    }
    Ok(existed)
}

/// Delete what an earlier install left behind: the `.old` copies the running
/// app held (Windows), `.new` ones of an install cut short.
pub fn cleanup(target: &InstallTarget) {
    for path in [target.app.clone(), target.dir.join(SQLS_FILE)] {
        remove_path(&sibling(&path, "old"));
        remove_path(&sibling(&path, "new"));
    }
}

/// `dir/name` → `dir/name.<ext>`.
fn sibling(path: &Path, ext: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".");
    name.push(ext);
    path.with_file_name(name)
}

/// Delete a file or a whole directory, if there; errors ignored.
fn remove_path(path: &Path) {
    match path.symlink_metadata() {
        Ok(meta) if meta.is_dir() => {
            let _ = fs::remove_dir_all(path);
        }
        Ok(_) => {
            let _ = fs::remove_file(path);
        }
        Err(_) => {}
    }
}

/// Windows' antivirus briefly holds a freshly written executable open, which
/// makes a rename fail; a few tries ride it out.
fn rename_retry(from: &Path, to: &Path) -> Result<()> {
    let mut tries = 0;
    loop {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(_) if tries < 5 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => {
                return Err(anyhow!(e).context(format!(
                    "renaming {} to {}",
                    from.display(),
                    to.display()
                )))
            }
        }
    }
}

/// Copy a file (mode included) or, on macOS, a bundle (`ditto`: symlinks,
/// modes, extended attributes — what keeps its signature valid).
fn copy_path(src: &Path, dst: &Path) -> Result<()> {
    if src.is_dir() {
        return copy_dir(src, dst);
    }
    fs::copy(src, dst)?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    let status = std::process::Command::new("/usr/bin/ditto")
        .arg(src)
        .arg(dst)
        .status()
        .context("running ditto")?;
    if !status.success() {
        bail!("ditto could not copy {} ({status})", src.display());
    }
    Ok(())
}

/// Elsewhere only tests stage a directory.
#[cfg(not(target_os = "macos"))]
fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        copy_path(&entry.path(), &dst.join(entry.file_name()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead as _;
    use std::net::TcpListener;

    #[test]
    fn asset_names_cover_every_release_target() {
        assert_eq!(
            asset_name_for("linux", "x86_64"),
            Some("skimrow-linux-x64.zip")
        );
        assert_eq!(
            asset_name_for("linux", "aarch64"),
            Some("skimrow-linux-arm64.zip")
        );
        assert_eq!(
            asset_name_for("windows", "x86_64"),
            Some("skimrow-windows-x64.zip")
        );
        assert_eq!(
            asset_name_for("windows", "aarch64"),
            Some("skimrow-windows-arm64.zip")
        );
        assert_eq!(
            asset_name_for("macos", "aarch64"),
            Some("skimrow-macos-arm64.zip")
        );
        assert_eq!(asset_name_for("macos", "x86_64"), None);
        assert_eq!(asset_name_for("freebsd", "x86_64"), None);
    }

    #[test]
    fn api_url_from_the_repository() {
        let want = "https://api.github.com/repos/tochk/skimrow/releases/latest";
        assert_eq!(api_url("https://github.com/tochk/skimrow"), want);
        assert_eq!(api_url("https://github.com/tochk/skimrow/"), want);
        assert_eq!(api_url("https://github.com/tochk/skimrow.git"), want);
    }

    #[test]
    fn describe_versions_are_dev_builds() {
        assert!(is_dev_version("0.9.0-3-gabc1234"));
        assert!(is_dev_version("0.9.0-3-gabc1234-dirty"));
        assert!(is_dev_version("0.9.0-dirty"));
        assert!(is_dev_version("1.0.0-rc.1-12-g0123abcd"));
        assert!(!is_dev_version("0.9.0"));
        assert!(!is_dev_version("1.0.0-rc.1"));
        assert!(!is_dev_version("1.0.0-beta-2"));
    }

    #[test]
    fn newer_compares_from_the_described_tag() {
        let v = |s: &str| Version::parse(s).unwrap();
        assert!(is_newer("0.9.0", &v("0.10.0")));
        assert!(is_newer("0.9.0", &v("0.9.1")));
        assert!(!is_newer("0.9.0", &v("0.9.0")));
        assert!(!is_newer("0.10.0", &v("0.9.0")));
        // A build made after the 0.9.0 tag is not older than 0.9.0.
        assert!(!is_newer("0.9.0-3-gabc1234", &v("0.9.0")));
        assert!(is_newer("0.9.0-3-gabc1234-dirty", &v("0.9.1")));
        // A release candidate updates to its release, not to an older one.
        assert!(is_newer("1.0.0-rc.1", &v("1.0.0")));
        assert!(!is_newer("1.0.0-rc.1", &v("0.9.0")));
        assert!(!is_newer("not a version", &v("9.0.0")));
    }

    const RELEASE_JSON: &str = r#"{
        "tag_name": "v0.10.0",
        "html_url": "https://github.com/tochk/skimrow/releases/tag/v0.10.0",
        "draft": false,
        "prerelease": false,
        "assets": [
            {"name": "skimrow-linux-x64.zip", "size": 30177767,
             "browser_download_url": "https://github.com/tochk/skimrow/releases/download/v0.10.0/skimrow-linux-x64.zip",
             "digest": "sha256:ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef0123456789"},
            {"name": "skimrow-windows-arm64.zip", "size": 21021188,
             "browser_download_url": "https://github.com/tochk/skimrow/releases/download/v0.10.0/skimrow-windows-arm64.zip",
             "digest": null}
        ]
    }"#;

    #[test]
    fn parses_a_release() {
        let r = parse_release(RELEASE_JSON, Some("skimrow-linux-x64.zip")).unwrap();
        assert_eq!(r.version, Version::new(0, 10, 0));
        assert_eq!(
            r.page,
            "https://github.com/tochk/skimrow/releases/tag/v0.10.0"
        );
        let a = r.asset.unwrap();
        assert_eq!(a.size, 30177767);
        assert!(a.url.ends_with("/v0.10.0/skimrow-linux-x64.zip"));
        assert_eq!(
            a.sha256.as_deref(),
            Some("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789")
        );

        let r = parse_release(RELEASE_JSON, Some("skimrow-windows-arm64.zip")).unwrap();
        assert_eq!(r.asset.unwrap().sha256, None);
        let r = parse_release(RELEASE_JSON, Some("skimrow-macos-arm64.zip")).unwrap();
        assert_eq!(r.asset, None);
        assert!(parse_release(r#"{"tag_name": "nightly"}"#, None).is_err());
        assert!(parse_release("<html>", None).is_err());
    }

    #[test]
    fn digest_must_be_sha256() {
        assert_eq!(sha256_of_digest("md5:abc"), None);
        assert_eq!(sha256_of_digest("sha256:abc"), None);
    }

    #[test]
    fn bundle_of_an_executable() {
        assert_eq!(
            bundle_of(Path::new(
                "/Applications/skimrow.app/Contents/MacOS/skimrow"
            )),
            Some(PathBuf::from("/Applications/skimrow.app"))
        );
        assert_eq!(bundle_of(Path::new("/usr/local/bin/skimrow")), None);
        assert_eq!(bundle_of(Path::new("/x/y.app/Other/MacOS/skimrow")), None);
    }

    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(&mut out);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in entries {
            zip.start_file(*name, opts).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();
        out.into_inner()
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn unpack_takes_only_the_update_files() {
        let tmp = tempfile::tempdir().unwrap();
        let zip = tmp.path().join("u.zip");
        fs::write(
            &zip,
            zip_of(&[
                (SQLS_FILE, b"new sqls"),
                ("README.md", b"readme"),
                ("../evil", b"evil"),
                ("sub/skimrow", b"nested"),
                (APP_FILE, b"new app"),
            ]),
        )
        .unwrap();
        let dir = tmp.path().join("files");
        let names = unpack(&zip, &dir).unwrap();
        assert_eq!(names, vec![APP_FILE.to_string(), SQLS_FILE.to_string()]);
        assert_eq!(fs::read(dir.join(APP_FILE)).unwrap(), b"new app");
        assert!(!tmp.path().join("evil").exists());
        assert!(!dir.join("README.md").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(dir.join(APP_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "executable bits");
        }

        fs::write(&zip, zip_of(&[(SQLS_FILE, b"x")])).unwrap();
        assert!(unpack(&zip, &dir).is_err(), "no app in the archive");
        fs::write(&zip, b"not a zip").unwrap();
        assert!(unpack(&zip, &dir).is_err());
    }

    /// An install dir holding `files`, and its target.
    fn install_dir(root: &Path, files: &[(&str, &str)]) -> InstallTarget {
        let dir = root.join("install");
        fs::create_dir_all(&dir).unwrap();
        for (name, data) in files {
            fs::write(dir.join(name), data).unwrap();
        }
        InstallTarget::locate(&dir.join(APP_FILE)).unwrap()
    }

    fn staged(root: &Path, files: &[(&str, &str)]) -> Staged {
        let dir = root.join("staged");
        fs::create_dir_all(&dir).unwrap();
        for (name, data) in files {
            fs::write(dir.join(name), data).unwrap();
        }
        Staged {
            version: Version::new(9, 0, 0),
            dir,
            files: files.iter().map(|(n, _)| n.to_string()).collect(),
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn install_swaps_every_file_and_leaves_nothing_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let target = install_dir(
            tmp.path(),
            &[(APP_FILE, "old app"), (SQLS_FILE, "old sqls")],
        );
        target.check_writable().unwrap();
        let staged = staged(
            tmp.path(),
            &[(APP_FILE, "new app"), (SQLS_FILE, "new sqls")],
        );
        install(&staged, &target).unwrap();
        assert_eq!(fs::read_to_string(&target.app).unwrap(), "new app");
        assert_eq!(
            fs::read_to_string(target.dir.join(SQLS_FILE)).unwrap(),
            "new sqls"
        );
        let mut left: Vec<_> = fs::read_dir(&target.dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        let mut want = vec![APP_FILE.to_string(), SQLS_FILE.to_string()];
        want.sort();
        assert_eq!(left, want);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn install_adds_a_file_the_install_lacked() {
        let tmp = tempfile::tempdir().unwrap();
        let target = install_dir(tmp.path(), &[(APP_FILE, "old app")]);
        let staged = staged(
            tmp.path(),
            &[(APP_FILE, "new app"), (SQLS_FILE, "new sqls")],
        );
        install(&staged, &target).unwrap();
        assert_eq!(
            fs::read_to_string(target.dir.join(SQLS_FILE)).unwrap(),
            "new sqls"
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn failed_install_changes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let target = install_dir(
            tmp.path(),
            &[(APP_FILE, "old app"), (SQLS_FILE, "old sqls")],
        );
        let mut staged = staged(tmp.path(), &[(APP_FILE, "new app")]);
        // Listed but missing: the copy fails after the app's was made.
        staged.files.push(SQLS_FILE.to_string());
        assert!(install(&staged, &target).is_err());
        assert_eq!(fs::read_to_string(&target.app).unwrap(), "old app");
        assert_eq!(
            fs::read_dir(&target.dir).unwrap().count(),
            2,
            "no .new left"
        );
    }

    #[test]
    fn cleanup_removes_leftovers() {
        let tmp = tempfile::tempdir().unwrap();
        let target = install_dir(
            tmp.path(),
            &[
                (APP_FILE, "app"),
                (&format!("{APP_FILE}.old"), "x"),
                (&format!("{SQLS_FILE}.new"), "x"),
            ],
        );
        fs::create_dir(target.dir.join(format!("{SQLS_FILE}.old"))).unwrap();
        cleanup(&target);
        assert_eq!(fs::read_dir(&target.dir).unwrap().count(), 1);
        assert!(target.app.exists());
    }

    #[test]
    fn prune_keeps_only_newer_versions() {
        let tmp = tempfile::tempdir().unwrap();
        for dir in ["0.9.0", "0.10.0", "1.0.0", "junk"] {
            fs::create_dir_all(tmp.path().join(dir).join("files")).unwrap();
        }
        prune(tmp.path(), "0.10.0");
        let left: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(left, vec!["1.0.0".to_string()]);
    }

    /// Serve `bodies` to one connection each, in order, on a local port; returns
    /// its base URL. No internet involved.
    fn serve(bodies: Vec<Vec<u8>>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for body in bodies {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                    line.clear();
                }
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        format!("http://{addr}")
    }

    fn asset_for(url: String, data: &[u8]) -> Asset {
        Asset {
            name: "skimrow-test.zip".into(),
            url,
            size: data.len() as u64,
            sha256: Some(hex(&Sha256::digest(data))),
        }
    }

    #[test]
    fn fetches_the_latest_release() {
        let url = serve(vec![RELEASE_JSON.as_bytes().to_vec()]);
        let r = fetch_latest(&url, "skimrow-test", Some("skimrow-linux-x64.zip")).unwrap();
        assert_eq!(r.version, Version::new(0, 10, 0));
    }

    #[test]
    fn download_checks_size_and_digest() {
        let tmp = tempfile::tempdir().unwrap();
        let data = b"archive bytes".to_vec();
        let progress = AtomicU64::new(0);

        let url = serve(vec![data.clone()]);
        let path = download(&asset_for(url, &data), tmp.path(), "t", &progress).unwrap();
        assert_eq!(fs::read(&path).unwrap(), data);
        assert_eq!(progress.load(Ordering::Relaxed), data.len() as u64);

        let url = serve(vec![data.clone()]);
        let mut bad = asset_for(url, &data);
        bad.sha256 = Some("0".repeat(64));
        fs::remove_file(&path).unwrap();
        assert!(download(&bad, tmp.path(), "t", &progress).is_err());
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0, "nothing left");

        let url = serve(vec![data.clone()]);
        let mut short = asset_for(url, &data);
        short.sha256 = None;
        short.size += 10;
        assert!(download(&short, tmp.path(), "t", &progress).is_err());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn prepare_downloads_once_then_reuses_the_staging() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("update");
        fs::create_dir_all(cache.join("0.0.1/files")).unwrap();
        let data = zip_of(&[(APP_FILE, b"new app"), (SQLS_FILE, b"new sqls")]);
        // One connection only: a second download would fail.
        let url = serve(vec![data.clone()]);
        let release = Release {
            version: Version::new(9, 0, 0),
            page: String::new(),
            asset: Some(asset_for(url, &data)),
        };
        let progress = AtomicU64::new(0);
        let first = prepare(&release, &cache, "t", &progress).unwrap();
        assert_eq!(
            first.files,
            vec![APP_FILE.to_string(), SQLS_FILE.to_string()]
        );
        assert!(!cache.join("0.0.1").exists(), "older staging removed");
        assert!(
            !cache.join("9.0.0/skimrow-test.zip").exists(),
            "archive removed"
        );
        let again = prepare(&release, &cache, "t", &progress).unwrap();
        assert_eq!(again, first);

        let none = Release {
            asset: None,
            ..release
        };
        fs::remove_dir_all(&cache).unwrap();
        assert!(prepare(&none, &cache, "t", &progress).is_err());
    }
}
