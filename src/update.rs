//! Updates from GitHub releases. toss looks up the newest release, and on the user's click downloads its
//! build for this computer and checks it against SHA256SUMS, which the release workflow signs with the
//! release key below. toss downloads the files itself, so neither Windows nor macOS marks them as
//! downloaded and asks again on every update: toss's own check of the signature stands in for theirs.

use anyhow::{Context, Result, anyhow, ensure};
use iroh::{PublicKey, Signature};
use sha2::{Digest, Sha256};

pub const RELEASES: &str = "https://github.com/plamen-g/toss/releases";
/// The public half of the key that signs releases; the private half is the release workflow's
/// TOSS_RELEASE_KEY secret.
const RELEASE_KEY: &str = "919180b84019e475aeb863b27828a9ef8be5d1f438cc209a2d5a97ccd9148cdf";

/// This computer's build, as a release names it. None where toss doesn't replace itself (Linux, where
/// the update opens the release page instead).
pub fn asset() -> Option<&'static str> {
    if cfg!(windows) {
        Some("toss-windows-x86_64.exe")
    } else if cfg!(target_os = "macos") {
        Some("toss-macos.zip")
    } else {
        None
    }
}

/// The newest release's version, when it's newer than this toss.
pub async fn newer() -> Result<Option<String>> {
    // GitHub sends the latest release's page as a redirect, with its version in the address.
    let latest = client(false)?.get(format!("{RELEASES}/latest")).send().await?;
    let page = latest.headers().get(reqwest::header::LOCATION).context("GitHub didn't say which release is the latest")?.to_str()?;
    let version = page.rsplit_once("/tag/v").context("GitHub's latest release has an odd name")?.1;
    Ok(newer_than(version, env!("CARGO_PKG_VERSION")).then(|| version.to_string()))
}

/// "0.3.10" is newer than "0.3.9".
fn newer_than(version: &str, than: &str) -> bool {
    let numbers = |v: &str| v.split('.').map(|n| n.parse().unwrap_or(0)).collect::<Vec<u64>>();
    numbers(version) > numbers(than)
}

/// `version`'s build for this computer (`asset`), once it matches the release's signed checksums.
pub async fn download(version: &str, asset: &str) -> Result<Vec<u8>> {
    let client = client(true)?;
    let get = async |file: &str| -> Result<Vec<u8>> {
        let url = format!("{RELEASES}/download/v{version}/{file}");
        Ok(client.get(&url).send().await?.error_for_status().with_context(|| format!("couldn't download {file}"))?.bytes().await?.to_vec())
    };
    let (sums, sig) = (get("SHA256SUMS").await?, get("SHA256SUMS.sig").await?);
    let build = get(asset).await?;
    check(&build, asset, &sums, &sig, RELEASE_KEY)?;
    Ok(build)
}

/// Whether `build` is the file `name` as the checksum list `sums` has it, and `sig` is `key`'s signature of that list.
fn check(build: &[u8], name: &str, sums: &[u8], sig: &[u8], key: &str) -> Result<()> {
    let key: PublicKey = key.parse()?;
    let sig = Signature::from_bytes(sig.try_into().context("the release's signature has the wrong size")?);
    key.verify(sums, &sig).map_err(|_| anyhow!("the release isn't signed with toss's release key, so toss won't install it"))?;
    let hash: String = Sha256::digest(build).iter().map(|b| format!("{b:02x}")).collect();
    ensure!(lists(std::str::from_utf8(sums)?, &hash, name), "the download doesn't match the release's signed checksums");
    Ok(())
}

/// Whether sha256sum's output `sums` has `name` with `hash`: "<hash>  <name>", or "<hash> *<name>" where
/// it read the files as binary.
fn lists(sums: &str, hash: &str, name: &str) -> bool {
    sums.lines().any(|line| line.split_once(' ').is_some_and(|(h, rest)| h == hash && rest.strip_prefix([' ', '*']) == Some(name)))
}

fn client(follow_redirects: bool) -> Result<reqwest::Client> {
    // reqwest here has no TLS crypto of its own: it takes the one rustls already has for iroh.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let redirects = if follow_redirects { reqwest::redirect::Policy::limited(10) } else { reqwest::redirect::Policy::none() };
    Ok(reqwest::Client::builder()
        .user_agent(concat!("toss/", env!("CARGO_PKG_VERSION")))
        .redirect(redirects)
        .timeout(std::time::Duration::from_secs(600))
        .build()?)
}

/// Puts the downloaded build in place of this toss and starts it there; this toss gets asked to quit.
#[cfg(windows)]
pub fn install(build: &[u8]) -> Result<()> {
    let exe = std::env::temp_dir().join("toss-update.exe");
    std::fs::write(&exe, build)?;
    // As if the download were opened by hand: the new toss asks this one to quit and takes its place.
    crate::gui::no_console(std::process::Command::new(&exe).arg("--install")).spawn()?;
    Ok(())
}

/// Swaps toss.app for the downloaded one and starts it once this toss has quit (which the caller does).
#[cfg(target_os = "macos")]
pub fn install(build: &[u8]) -> Result<()> {
    use std::process::{Command, Stdio};
    let app = bundle().context("this toss isn't in a toss.app, so it can't replace itself")?;
    let unpacked = std::env::temp_dir().join(format!("toss-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&unpacked);
    std::fs::create_dir_all(&unpacked)?;
    let zip = unpacked.join("toss-macos.zip");
    std::fs::write(&zip, build)?;
    ensure!(Command::new("ditto").args(["-x", "-k"]).arg(&zip).arg(&unpacked).status()?.success(), "couldn't unpack the update");
    // A running app's files stay usable after a move, so the old one steps aside for the new one.
    let old = app.with_extension("old");
    let _ = std::fs::remove_dir_all(&old);
    std::fs::rename(&app, &old)?;
    if !Command::new("ditto").arg(unpacked.join("toss.app")).arg(&app).status()?.success() {
        std::fs::rename(&old, &app)?;
        anyhow::bail!("couldn't put the update in place");
    }
    let _ = std::fs::remove_dir_all(&unpacked);
    Command::new(app.join("Contents/MacOS/toss")).arg("--updated").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    Ok(())
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn install(_: &[u8]) -> Result<()> {
    anyhow::bail!("toss doesn't replace itself here: get the new version from {RELEASES}")
}

/// The toss.app this toss runs from.
#[cfg(target_os = "macos")]
pub fn bundle() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?; // .../toss.app/Contents/MacOS/toss
    let app = exe.parent()?.parent()?.parent()?;
    (app.extension()? == "app").then(|| app.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "asks GitHub: cargo test -- --ignored"]
    async fn github_says_which_release_is_newest() {
        let newer = newer().await.unwrap();
        assert!(newer.as_deref().is_none_or(|v| newer_than(v, env!("CARGO_PKG_VERSION"))), "{newer:?}");
    }

    #[test]
    fn newer_versions_by_number_not_text() {
        assert!(newer_than("0.3.10", "0.3.9"));
        assert!(newer_than("1.0.0", "0.9.9"));
        assert!(!newer_than("0.3.0", "0.3.0"));
        assert!(!newer_than("0.2.9", "0.3.0"));
    }

    #[test]
    fn updates_must_match_the_signed_list() {
        // Made with OpenSSL, as the release workflow signs: `openssl pkeyutl -sign -rawin`, with a test key.
        let key = "0073d8f86c62bcc341f0e5109ab3a67d9fdc871ec69f7b53b8f0e188ae8ee6cc";
        let sums = b"44575cf5b28512d75644bf54a517dcef304ff809fd511747621b4d64f19aac66  toss-windows-x86_64.exe\n";
        let sig: Vec<u8> = (0..128)
            .step_by(2)
            .map(|i| u8::from_str_radix(&"282b77903c9095df12d1cde92f1d13d31e0c51c1b59e6e3d2609d6b16910297516bcff2620c5c823f553be25f97fe693bf11407dc160c90de1e5d9b43a8df003"[i..i + 2], 16).unwrap())
            .collect();
        check(b"build", "toss-windows-x86_64.exe", sums, &sig, key).unwrap();
        assert!(check(b"other", "toss-windows-x86_64.exe", sums, &sig, key).is_err(), "a different build");
        assert!(check(b"build", "toss-macos.zip", sums, &sig, key).is_err(), "another platform's file");
        let mut forged = sums.to_vec();
        forged[0] = b'5';
        assert!(check(b"build", "toss-windows-x86_64.exe", &forged, &sig, key).is_err(), "a changed list");
        assert!(check(b"build", "toss-windows-x86_64.exe", sums, &sig, RELEASE_KEY).is_err(), "signed by another key");
        for line in ["abc  toss.zip", "abc *toss.zip"] {
            assert!(lists(&format!("def  other\n{line}\n"), "abc", "toss.zip"), "{line}");
        }
        assert!(!lists("abc  toss.zip.old", "abc", "toss.zip"));
    }
}
