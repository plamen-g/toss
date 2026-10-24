//! toss: send files to your own devices and friends, peer to peer.
//! Devices are known by public key (iroh), never by IP address.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::{IsTerminal as _, Write as _},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use iroh::{
    Endpoint, EndpointId, SecretKey, Signature,
    endpoint::{Connection, Incoming, PortmapperConfig, presets},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const ALPN: &[u8] = b"toss/1";
const INVITE_ALPN: &[u8] = b"toss/invite/1";
const DIALER: &[u8] = b"dialer";
const ACCEPTOR: &[u8] = b"acceptor";
const MAX_MSG: usize = 16 << 20;

const USAGE: &str = "usage:
  toss invite <name>          print an invite link (<name> is what you'll call them)
  toss join <link> <name>     accept an invite link (<name> is what you'll call them)
  toss contacts               list contacts
  toss forget <name>          remove a contact
  toss send <name> <path>...  send files or folders to a contact
  toss listen                 receive files (keep it running)";

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let res = match args.as_slice() {
        ["invite", name] => invite(name).await,
        ["join", link, name] => join(link, name).await,
        ["contacts"] => list(),
        ["forget", name] => forget(name),
        ["send", name, paths @ ..] if !paths.is_empty() => send(name, paths).await,
        ["listen"] => listen().await,
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2)
        }
    };
    if let Err(e) = res {
        eprintln!("error: {e:#}");
        std::process::exit(1)
    }
}

#[derive(Serialize, Deserialize)]
struct Contact {
    id: String,
    #[serde(default)]
    auto_accept: bool,
}
type Contacts = BTreeMap<String, Contact>;

/// First message on every connection: who I am, proven by signing this connection's TLS keys.
/// The same message carries the invite secret (join) or the file list (send).
#[derive(Serialize, Deserialize)]
struct Hello {
    from: String,
    sig: Vec<u8>,
    #[serde(default)]
    secret: Vec<u8>,
    #[serde(default)]
    files: Vec<(String, u64)>, // '/'-separated relative path, size in bytes
}

async fn invite(name: &str) -> Result<()> {
    let home = home()?;
    let me = load_key(&home)?;
    ensure!(!load_contacts(&home)?.contains_key(name), "you already have a contact called {name}");
    // A throwaway key, so a leaked link never reveals your real device id.
    let ep = bind(SecretKey::generate(), &[INVITE_ALPN]).await?;
    tokio::time::timeout(Duration::from_secs(15), ep.online())
        .await
        .context("can't reach the internet, so nobody could join")?;
    let secret = SecretKey::generate().to_bytes()[..16].to_vec(); // 16 bytes from the OS random generator
    println!("Send this to {name}. It works once, while this command runs:\n");
    println!("  toss join toss:{}/{} <name for you>\n", ep.id(), hex(&secret));
    println!("Waiting for {name} to join...");
    while let Some(incoming) = ep.accept().await {
        // Relays and the lookup server see the throwaway id, so only the secret proves it's really them.
        match tokio::time::timeout(Duration::from_secs(30), accept_join(incoming, &me, &secret)).await {
            Ok(Ok(peer)) => {
                add_contact(&home, name, peer)?;
                println!("Paired with {name}.");
                break;
            }
            Ok(Err(e)) => eprintln!("Rejected a join attempt: {e:#}"),
            Err(_) => eprintln!("Rejected a join attempt: timed out"),
        }
    }
    ep.close().await;
    Ok(())
}

async fn accept_join(incoming: Incoming, me: &SecretKey, secret: &[u8]) -> Result<EndpointId> {
    let conn = incoming.await?;
    let (mut tx, mut rx) = conn.accept_bi().await?;
    let h: Hello = recv_msg(&mut rx).await?;
    ensure!(same(&h.secret, secret), "wrong invite secret");
    let peer = verify(&conn, &h, DIALER)?;
    send_msg(&mut tx, &introduce(&conn, me, ACCEPTOR)?).await?;
    tx.finish()?;
    let _ = tokio::time::timeout(Duration::from_secs(10), conn.closed()).await; // let them read the reply
    Ok(peer)
}

async fn join(link: &str, name: &str) -> Result<()> {
    let home = home()?;
    let me = load_key(&home)?;
    ensure!(!load_contacts(&home)?.contains_key(name), "you already have a contact called {name}");
    let (id, secret) = parse_link(link)?;
    let ep = bind(SecretKey::generate(), &[]).await?;
    let conn = ep
        .connect(id, INVITE_ALPN)
        .await
        .context("couldn't reach the inviter. Is their `toss invite` still running?")?;
    let (mut tx, mut rx) = conn.open_bi().await?;
    send_msg(&mut tx, &Hello { secret, ..introduce(&conn, &me, DIALER)? }).await?;
    let peer = verify(&conn, &recv_msg(&mut rx).await?, ACCEPTOR)?;
    add_contact(&home, name, peer)?;
    println!("Paired with {name}.");
    ep.close().await;
    Ok(())
}

fn list() -> Result<()> {
    let contacts = load_contacts(&home()?)?;
    if contacts.is_empty() {
        println!("No contacts yet. Pair with `toss invite <name>`.");
    }
    for (name, c) in &contacts {
        println!("{name}{}", if c.auto_accept { "  (auto-accept)" } else { "" });
    }
    Ok(())
}

fn forget(name: &str) -> Result<()> {
    update_contacts(&home()?, |c| {
        ensure!(c.remove(name).is_some(), "no contact called {name}");
        Ok(())
    })?;
    println!("Forgot {name}.");
    Ok(())
}

async fn send(name: &str, paths: &[&str]) -> Result<()> {
    let home = home()?;
    let me = load_key(&home)?;
    let peer: EndpointId = load_contacts(&home)?
        .get(name)
        .with_context(|| format!("no contact called {name}"))?
        .id
        .parse()?;
    let mut files = Vec::new(); // (relative path, local path, size)
    let mut tops = HashSet::new();
    for p in paths.iter().map(Path::new) {
        let top = p.file_name().and_then(|n| n.to_str()).with_context(|| format!("can't send {}", p.display()))?;
        ensure!(tops.insert(top.to_lowercase()), "two of the items are called {top}; rename one");
        let meta = std::fs::metadata(p).with_context(|| format!("can't read {}", p.display()))?;
        if meta.is_dir() {
            walk(p, top, &mut files)?
        } else {
            files.push((top.to_string(), p.to_path_buf(), meta.len()))
        }
    }
    ensure!(!files.is_empty(), "nothing to send (empty folder?)");
    let total = files.iter().map(|f| f.2).sum();

    // Dial from a throwaway key so a `toss listen` on this machine keeps the real one. The signature proves who we are.
    let ep = bind(SecretKey::generate(), &[]).await?;
    let conn = ep
        .connect(peer, ALPN)
        .await
        .with_context(|| format!("couldn't reach {name}. Is `toss listen` running there?"))?;
    let (mut tx, mut rx) = conn.open_bi().await?;
    let files_msg = files.iter().map(|(rel, _, size)| (rel.clone(), *size)).collect();
    send_msg(&mut tx, &Hello { files: files_msg, ..introduce(&conn, &me, DIALER)? }).await?;
    println!("Waiting for {name} to accept...");
    if !matches!(rx.read_u8().await, Ok(1)) {
        bail!("{name} declined (or doesn't have you as a contact)");
    }
    let mut progress = Progress::new(total);
    let started = Instant::now();
    for (_, path, size) in &files {
        let file = tokio::fs::File::open(path).await?;
        copy_exact(file, &mut tx, *size, &mut progress)
            .await
            .with_context(|| format!("sending {}", path.display()))?;
    }
    tx.finish()?;
    ensure!(matches!(rx.read_u8().await, Ok(1)), "{name} didn't confirm the files arrived");
    progress.finish();
    let route = if conn.paths().iter().any(|p| p.is_selected() && p.is_ip()) { "direct" } else { "via relay" };
    let speed = match mb(total) / started.elapsed().as_secs_f64() {
        s if total >= 1_000_000 => format!(", {s:.1} MB/s"),
        _ => String::new(), // too small to time meaningfully
    };
    println!("Sent {} file(s) to {name} ({route}{speed}).", files.len());
    ep.close().await;
    Ok(())
}

fn walk(dir: &Path, rel: &str, out: &mut Vec<(String, PathBuf, u64)>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().into_string().map_err(|n| anyhow!("can't send non-UTF-8 name {n:?}"))?;
        let rel = format!("{rel}/{name}");
        let kind = entry.file_type()?;
        // Symlinks are skipped: following them can loop or pull in files from outside the folder.
        if kind.is_dir() {
            walk(&entry.path(), &rel, out)?
        } else if kind.is_file() {
            out.push((rel, entry.path(), entry.metadata()?.len()))
        }
    }
    Ok(())
}

async fn listen() -> Result<()> {
    let home = home()?;
    let me = load_key(&home)?;
    let downloads = dirs::download_dir().or_else(dirs::home_dir).context("no downloads folder")?;
    // Two listeners with one key fight over it: the newest silently gets every transfer.
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(home.join("listen.lock"))?;
    ensure!(lock.try_lock().is_ok(), "toss listen is already running on this device");
    let ep = bind(me, &[ALPN]).await?;
    println!("Listening. Files go to {}. Ctrl+C to stop.", downloads.display());
    while let Some(incoming) = ep.accept().await {
        // shortcut: one transfer at a time so accept prompts don't interleave; fine until there's a GUI.
        if let Err(e) = receive(incoming, &home, &downloads).await {
            eprintln!("Transfer failed: {e:#}");
        }
    }
    Ok(())
}

async fn receive(incoming: Incoming, home: &Path, downloads: &Path) -> Result<()> {
    let conn = incoming.await?;
    // Anyone who learns our id can connect, so a silent connection must not block the queue.
    let (mut tx, mut rx, offer) = tokio::time::timeout(Duration::from_secs(30), async {
        let (tx, mut rx) = conn.accept_bi().await?;
        let offer: Hello = recv_msg(&mut rx).await?;
        anyhow::Ok((tx, rx, offer))
    })
    .await
    .context("timed out waiting for the file list")??;
    let from = verify(&conn, &offer, DIALER)?.to_string();
    let contacts = load_contacts(home)?; // reload: pairing may have happened in another process
    let name = contacts
        .iter()
        .find(|(_, c)| c.id == from)
        .map(|(n, _)| n.clone())
        .context("ignored a transfer from a device that isn't a contact")?;
    let dests = plan(&offer.files, downloads, |p| p.exists())?;

    let total = offer.files.iter().map(|f| f.1).sum();
    println!("\n{name} wants to send {} file(s), {:.1} MB:", offer.files.len(), mb(total));
    // Show the cleaned names that will be saved, never the raw ones: they could hide terminal control codes.
    for (dest, (_, size)) in dests.iter().zip(&offer.files).take(10) {
        println!("  {} ({:.1} MB)", dest.strip_prefix(downloads).unwrap_or(dest).display(), mb(*size));
    }
    if offer.files.len() > 10 {
        println!("  ...and {} more", offer.files.len() - 10);
    }
    let accepted = contacts[&name].auto_accept
        || match ask("Accept? [y]es / [n]o / [a]lways from this contact: ").await?.as_str() {
            "y" | "yes" => true,
            "a" | "always" => {
                update_contacts(home, |c| {
                    if let Some(c) = c.get_mut(&name) {
                        c.auto_accept = true; // not if they were forgotten while we waited
                    }
                    Ok(())
                })?;
                true
            }
            _ => false,
        };
    tx.write_u8(accepted as u8).await?;
    if !accepted {
        println!("Declined.");
        return Ok(());
    }
    let mut progress = Progress::new(total);
    for ((_, size), dest) in offer.files.iter().zip(&dests) {
        receive_file(&mut rx, dest, *size, &mut progress).await?;
    }
    tx.write_u8(1).await?;
    tx.finish()?;
    progress.finish();
    println!("Received {} file(s) from {name}.", offer.files.len());
    let _ = tokio::time::timeout(Duration::from_secs(5), conn.closed()).await; // let the confirmation arrive
    Ok(())
}

async fn receive_file(rx: &mut (impl AsyncRead + Unpin), dest: &Path, size: u64, p: &mut Progress) -> Result<()> {
    if let Some(dir) = dest.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    // Write under a temporary name so a failed transfer never looks like a finished file.
    let name = dest.file_name().expect("checked by plan").to_string_lossy();
    let part = dest.with_file_name(format!("{name}.toss-part"));
    let res = async {
        let mut file = tokio::fs::File::create(&part).await?;
        copy_exact(&mut *rx, &mut file, size, p).await?;
        file.flush().await?;
        drop(file); // Windows can't rename an open file
        tokio::fs::rename(&part, dest).await?;
        anyhow::Ok(())
    }
    .await;
    if res.is_err() {
        let _ = tokio::fs::remove_file(&part).await;
    }
    res
}

/// Turn the sender's paths into destinations inside `downloads`.
/// Each top-level file or folder gets a name that's free, so nothing existing is ever overwritten.
fn plan(files: &[(String, u64)], downloads: &Path, exists: impl Fn(&Path) -> bool) -> Result<Vec<PathBuf>> {
    let (mut seen, mut used, mut tops) = (HashSet::new(), HashSet::new(), HashMap::new());
    let mut out = Vec::new();
    for (rel, _) in files {
        let segs = rel.split('/').map(safe_name).collect::<Result<Vec<_>>>()?;
        ensure!(seen.insert(segs.join("/").to_lowercase()), "duplicate path in offer: {rel}");
        let top = &segs[0];
        if !tops.contains_key(top) {
            let name = free_name(top, |n| exists(&downloads.join(n)) || used.contains(&n.to_lowercase()));
            used.insert(name.to_lowercase());
            tops.insert(top.clone(), name);
        }
        let mut dest = downloads.join(&tops[top]);
        dest.extend(&segs[1..]);
        out.push(dest);
    }
    Ok(out)
}

/// One part of a path from the other device, turned into a plain file name that works on every OS.
/// Separators can't survive, so nothing can land outside the downloads folder.
fn safe_name(seg: &str) -> Result<String> {
    ensure!(!matches!(seg, "" | "." | ".."), "unsafe path in offer");
    let mut name: String =
        seg.chars().map(|c| if c.is_control() || r#"\/:*?"<>|"#.contains(c) { '_' } else { c }).collect();
    if name.ends_with(['.', ' ']) {
        name.push('_'); // Windows silently strips these, which could merge two names
    }
    let stem = name.split('.').next().unwrap_or_default().to_ascii_uppercase();
    let numbered = stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.ends_with(|c: char| c.is_ascii_digit());
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") || numbered {
        name.insert(stem.len(), '_'); // Windows device names: "con.txt" -> "con_.txt"
    }
    ensure!(
        matches!(Path::new(&name).components().collect::<Vec<_>>()[..], [Component::Normal(_)]),
        "unsafe path in offer"
    );
    Ok(name)
}

/// "report.pdf" -> "report (1).pdf" -> "report (2).pdf" ... until `taken` says no.
fn free_name(name: &str, taken: impl Fn(&str) -> bool) -> String {
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (name, String::new()),
    };
    (0..)
        .map(|i| if i == 0 { name.to_string() } else { format!("{stem} ({i}){ext}") })
        .find(|n| !taken(n))
        .expect("infinite candidates")
}

async fn copy_exact(
    mut from: impl AsyncRead + Unpin,
    to: &mut (impl AsyncWrite + Unpin),
    len: u64,
    progress: &mut Progress,
) -> Result<()> {
    let mut buf = vec![0; 256 * 1024];
    let mut left = len;
    while left > 0 {
        let chunk = left.min(buf.len() as u64) as usize;
        let n = from.read(&mut buf[..chunk]).await?;
        ensure!(n > 0, "data ended early");
        to.write_all(&buf[..n]).await?;
        left -= n as u64;
        progress.add(n as u64);
    }
    Ok(())
}

struct Progress {
    done: u64,
    total: u64,
    last: Instant,
    tty: bool, // no live progress in logs (systemd), only the final line
}

impl Progress {
    fn new(total: u64) -> Self {
        Self { done: 0, total, last: Instant::now(), tty: std::io::stdout().is_terminal() }
    }
    fn add(&mut self, n: u64) {
        self.done += n;
        if self.tty && self.last.elapsed() > Duration::from_millis(250) {
            self.last = Instant::now();
            self.print();
        }
    }
    fn print(&self) {
        print!("\r  {:.1} / {:.1} MB", mb(self.done), mb(self.total));
        let _ = std::io::stdout().flush();
    }
    fn finish(&self) {
        if self.tty {
            self.print();
            println!();
        }
    }
}

fn introduce(conn: &Connection, me: &SecretKey, role: &[u8]) -> Result<Hello> {
    let sig = me.sign(&binding(conn, role)?).to_bytes().to_vec();
    Ok(Hello { from: me.public().to_string(), sig, secret: vec![], files: vec![] })
}

fn verify(conn: &Connection, h: &Hello, role: &[u8]) -> Result<EndpointId> {
    let from: EndpointId = h.from.parse()?;
    let sig = Signature::from_bytes(h.sig.as_slice().try_into().context("bad signature")?);
    from.verify(&binding(conn, role)?, &sig).map_err(|_| anyhow!("identity check failed"))?;
    Ok(from)
}

/// Bytes unique to this encrypted connection, so a signature can't be replayed on another one.
fn binding(conn: &Connection, role: &[u8]) -> Result<[u8; 32]> {
    let mut out = [0; 32];
    conn.export_keying_material(&mut out, b"toss/1 identity", role)
        .map_err(|e| anyhow!("{e:?}"))?;
    Ok(out)
}

async fn bind(key: SecretKey, alpns: &[&[u8]]) -> Result<Endpoint> {
    Ok(Endpoint::builder(presets::N0)
        .secret_key(key)
        .alpns(alpns.iter().map(|a| a.to_vec()).collect())
        .portmapper_config(PortmapperConfig::Disabled) // UPnP probing can trigger firewall prompts
        .bind()
        .await?)
}

async fn send_msg(w: &mut (impl AsyncWrite + Unpin), msg: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(msg)?;
    w.write_u32(bytes.len() as u32).await?;
    w.write_all(&bytes).await?;
    Ok(())
}

async fn recv_msg<T: DeserializeOwned>(r: &mut (impl AsyncRead + Unpin)) -> Result<T> {
    let len = r.read_u32().await? as usize;
    ensure!(len <= MAX_MSG, "message too large");
    let mut bytes = vec![0; len];
    r.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

async fn ask(question: &str) -> Result<String> {
    print!("{question}");
    std::io::stdout().flush()?;
    let line = tokio::task::spawn_blocking(|| {
        let mut s = String::new();
        std::io::stdin().read_line(&mut s).map(|_| s)
    })
    .await??;
    Ok(line.trim().to_lowercase())
}

fn home() -> Result<PathBuf> {
    let dir = match std::env::var_os("TOSS_HOME") {
        Some(dir) => dir.into(),
        None => dirs::config_dir().context("no config folder")?.join("toss"),
    };
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn load_key(home: &Path) -> Result<SecretKey> {
    let path = home.join("key");
    if let Ok(bytes) = std::fs::read(&path) {
        return Ok(SecretKey::from_bytes(bytes.as_slice().try_into().context("corrupt key file")?));
    }
    let key = SecretKey::generate();
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    opts.open(&path)?.write_all(&key.to_bytes())?;
    Ok(key)
}

fn load_contacts(home: &Path) -> Result<Contacts> {
    match std::fs::read(home.join("contacts.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("contacts.json is corrupt"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Contacts::new()),
        Err(e) => Err(e.into()),
    }
}

/// Re-reads contacts right before changing them, so edits made meanwhile by another toss command survive.
// shortcut: last writer wins if two toss commands save at the same moment; fine for one person.
fn update_contacts(home: &Path, change: impl FnOnce(&mut Contacts) -> Result<()>) -> Result<()> {
    let mut contacts = load_contacts(home)?;
    change(&mut contacts)?;
    let tmp = home.join("contacts.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&contacts)?)?;
    std::fs::rename(tmp, home.join("contacts.json"))?; // a crash never leaves a half-written file
    Ok(())
}

fn add_contact(home: &Path, name: &str, peer: EndpointId) -> Result<()> {
    update_contacts(home, |c| {
        c.insert(name.into(), Contact { id: peer.to_string(), auto_accept: false });
        Ok(())
    })
}

fn parse_link(link: &str) -> Result<(EndpointId, Vec<u8>)> {
    let (id, secret) = link
        .trim()
        .strip_prefix("toss:")
        .and_then(|s| s.split_once('/'))
        .context("not a toss invite link")?;
    Ok((id.parse()?, unhex(secret).context("not a toss invite link")?))
}

fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |diff, (x, y)| diff | (x ^ y)) == 0 // constant time
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / 1e6
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_stay_inside_downloads_and_work_on_every_os() {
        let dl = Path::new("dl");
        for bad in ["", "..", "a/../b", "/etc/passwd", "a//b", "dir/."] {
            assert!(plan(&[(bad.into(), 1)], dl, |_| false).is_err(), "{bad:?} should be rejected");
        }
        for (from, to) in [
            ("a.txt", "a.txt"),
            (".hidden", ".hidden"),
            ("backup 10:30.tar", "backup 10_30.tar"),
            ("C:evil", "C_evil"),
            ("a\\..\\b", "a_.._b"),
            ("what?.pdf", "what_.pdf"),
            ("tab\there", "tab_here"),
            ("trail.", "trail._"),
            ("con", "con_"),
            ("aux.c", "aux_.c"),
            ("COM1.log", "COM1_.log"),
            ("company.txt", "company.txt"),
        ] {
            assert_eq!(safe_name(from).unwrap(), to);
        }
    }

    /// Two endpoints in this process, connected directly (no relays, no internet).
    async fn connected_pair() -> (Connection, Connection, Endpoint, Endpoint) {
        let server = Endpoint::builder(presets::Minimal).alpns(vec![ALPN.to_vec()]).bind().await.unwrap();
        let client = Endpoint::builder(presets::Minimal).bind().await.unwrap();
        let port = server.bound_sockets().iter().find(|s| s.is_ipv4()).unwrap().port();
        let addr = iroh::EndpointAddr::new(server.id()).with_ip_addr(([127, 0, 0, 1], port).into());
        let (dialed, accepted) =
            tokio::join!(client.connect(addr, ALPN), async { server.accept().await.unwrap().await });
        (dialed.unwrap(), accepted.unwrap(), client, server)
    }

    #[tokio::test]
    async fn identity_proof_only_works_on_its_own_connection() {
        let me = SecretKey::generate();
        let (dialed, accepted, _c1, _s1) = connected_pair().await;
        let hello = introduce(&dialed, &me, DIALER).unwrap();
        assert_eq!(verify(&accepted, &hello, DIALER).unwrap(), me.public());
        assert!(verify(&accepted, &hello, ACCEPTOR).is_err(), "wrong role must fail");
        let (_, other, _c2, _s2) = connected_pair().await;
        assert!(verify(&other, &hello, DIALER).is_err(), "replay on another connection must fail");
    }

    #[test]
    fn never_overwrites_or_collides() {
        let dl = Path::new("dl");
        let files = [("a.txt".into(), 1), ("a (1).txt".into(), 1), ("dir/x".into(), 1), ("dir/y".into(), 1)];
        let dests = plan(&files, dl, |p| p == dl.join("a.txt") || p == dl.join("dir")).unwrap();
        assert_eq!(dests, [dl.join("a (1).txt"), dl.join("a (1) (1).txt"), dl.join("dir (1)/x"), dl.join("dir (1)/y")]);
        assert!(plan(&[("A.txt".into(), 1), ("a.txt".into(), 1)], dl, |_| false).is_err());
    }

    #[test]
    fn invite_link_roundtrip() {
        let id = SecretKey::generate().public();
        let (got_id, got_secret) = parse_link(&format!(" toss:{id}/{} ", hex(&[7; 16]))).unwrap();
        assert_eq!((got_id, got_secret), (id, vec![7; 16]));
        assert!(parse_link("toss:nope").is_err());
    }
}
