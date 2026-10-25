//! toss: send files to your own devices and friends, peer to peer.
//! Devices are known by public key (iroh), never by IP address.

mod gui;

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    future::Future,
    io::{IsTerminal as _, Write as _},
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use iroh::{
    Endpoint, EndpointId, SecretKey, Signature,
    endpoint::{Connection, Incoming, PortmapperConfig, VarInt, WeakConnectionHandle, presets},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt};

// v2 adds resuming; the version is in the protocol name so mismatched builds fail cleanly.
const ALPN: &[u8] = b"toss/2";
const INVITE_ALPN: &[u8] = b"toss/invite/1";
const DIALER: &[u8] = b"dialer";
const ACCEPTOR: &[u8] = b"acceptor";
const MAX_MSG: usize = 16 << 20;
// The receiver's first reply to an offer.
const DECLINED: u8 = 0;
const ACCEPTED: u8 = 1;
const NO_SPACE: u8 = 2;

const USAGE: &str = "usage:
  toss                        open the window
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
        [] => gui::run(),
        ["invite", name] => cli_invite(name).await,
        ["join", link, name] => join(link, name).await.map(|()| println!("Paired with {name}.")),
        ["contacts"] => list(),
        ["forget", name] => forget(name).map(|()| println!("Forgot {name}.")),
        ["send", name, paths @ ..] if !paths.is_empty() => send(name, paths, &Cli).await,
        ["listen"] => listen(Arc::new(Cli)).await,
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

enum Answer {
    No,
    Yes,
    Always,
}

/// What the core reports while it works. The terminal prints it; the window shows it.
enum Event<'a> {
    Ready(&'a Path),
    Waiting { to: &'a str, files: usize },
    Sent { to: &'a str, files: usize, direct: bool, mb_per_s: Option<f64>, resumed: u64 },
    Receiving { from: &'a str, files: usize },
    Checking { bytes: u64 }, // hashing what an earlier try left before continuing; minutes on a slow disk
    Received { from: &'a str, files: usize, resumed: u64 },
    Declined { from: &'a str },
    Failed(String),
}

/// What differs between the terminal and the window: how events and progress show, and how we ask.
trait Ui: Sync {
    fn event(&self, event: Event);
    fn progress(&self, done: u64, total: u64);
    fn accept(&self, from: &str, files: &[(String, u64)]) -> impl Future<Output = Answer> + Send;
}

struct Cli;

impl Ui for Cli {
    fn event(&self, event: Event) {
        match event {
            Event::Ready(dir) => println!("Ready to receive. Files go to {}.", dir.display()),
            Event::Waiting { to, .. } => println!("Waiting for {to} to accept..."),
            Event::Sent { to, files, direct, mb_per_s, resumed } => {
                let mut notes = vec![if direct { "direct" } else { "via relay" }.to_string()];
                notes.extend(mb_per_s.map(|s| format!("{s:.1} MB/s")));
                notes.extend(continued(resumed));
                println!("Sent {} to {to} ({}).", count(files), notes.join(", "));
            }
            Event::Receiving { from, files } => println!("Receiving {} from {from}...", count(files)),
            Event::Checking { bytes } => println!("Checking {} from an earlier try...", human(bytes)),
            Event::Received { from, files, resumed } => match continued(resumed) {
                Some(note) => println!("Received {} from {from} ({note}).", count(files)),
                None => println!("Received {} from {from}.", count(files)),
            },
            Event::Declined { from } => println!("Declined {from}'s files."),
            Event::Failed(why) => println!("Transfer failed: {why}"),
        }
    }

    fn progress(&self, done: u64, total: u64) {
        if !std::io::stdout().is_terminal() {
            return; // no live progress in logs (systemd)
        }
        print!("\r  {} / {}   ", human(done), human(total));
        if done == total {
            println!();
        }
        let _ = std::io::stdout().flush();
    }

    fn accept(&self, from: &str, files: &[(String, u64)]) -> impl Future<Output = Answer> + Send {
        let total: u64 = files.iter().map(|f| f.1).sum();
        println!("\n{from} wants to send {}, {}:", count(files.len()), human(total));
        for (name, size) in files.iter().take(10) {
            println!("  {name} ({})", human(*size));
        }
        if files.len() > 10 {
            println!("  ...and {} more", files.len() - 10);
        }
        async {
            match ask("Accept? [y]es / [n]o / [a]lways from this contact: ").await.as_deref() {
                Ok("y" | "yes") => Answer::Yes,
                Ok("a" | "always") => Answer::Always,
                _ => Answer::No,
            }
        }
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

async fn cli_invite(name: &str) -> Result<()> {
    let (link, joined) = invite(name).await?;
    println!("Send this to {name}. It works once, while this command runs:\n\n  toss join {link} <name for you>\n");
    println!("Waiting for {name} to join...");
    joined.await?;
    println!("Paired with {name}.");
    Ok(())
}

/// Returns the link to share, and a future that finishes once they've joined.
async fn invite(name: &str) -> Result<(String, impl Future<Output = Result<()>> + Send + 'static)> {
    let home = home()?;
    let me = load_key(&home)?;
    ensure!(!load_contacts(&home)?.contains_key(name), "you already have a contact called {name}");
    // A throwaway key, so a leaked link never reveals your real device id.
    let ep = bind(SecretKey::generate(), &[INVITE_ALPN]).await?;
    tokio::time::timeout(Duration::from_secs(15), ep.online())
        .await
        .context("can't reach the internet, so nobody could join")?;
    let secret = SecretKey::generate().to_bytes()[..16].to_vec(); // 16 bytes from the OS random generator
    let link = format!("toss:{}/{}", ep.id(), hex(&secret));
    let name = name.to_string();
    let joined = async move {
        while let Some(incoming) = ep.accept().await {
            // Relays and the lookup server see the throwaway id, so only the secret proves it's really them.
            match tokio::time::timeout(Duration::from_secs(30), accept_join(incoming, &me, &secret)).await {
                Ok(Ok(peer)) => {
                    add_contact(&home, &name, peer)?;
                    ep.close().await;
                    return Ok(());
                }
                Ok(Err(e)) => eprintln!("Rejected a join attempt: {e:#}"),
                Err(_) => eprintln!("Rejected a join attempt: timed out"),
            }
        }
        bail!("stopped before anyone joined")
    };
    Ok((link, joined))
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
    })
}

async fn send(name: &str, paths: &[impl AsRef<Path>], ui: &impl Ui) -> Result<()> {
    let home = home()?;
    let me = load_key(&home)?;
    let peer: EndpointId = load_contacts(&home)?
        .get(name)
        .with_context(|| format!("no contact called {name}"))?
        .id
        .parse()?;
    let mut files = Vec::new(); // (relative path, local path, size)
    let mut tops = HashSet::new();
    for p in paths.iter().map(|p| p.as_ref()) {
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
    ui.event(Event::Waiting { to: name, files: files.len() });
    match rx.read_u8().await {
        Ok(ACCEPTED) => {}
        Ok(NO_SPACE) => bail!("{name} doesn't have enough free disk space"),
        _ => bail!("{name} declined (or doesn't have you as a contact)"),
    }
    // They say how much of each file they already have, with a hash of it. Continue from there only
    // where our copy starts with exactly those bytes; anything else starts over.
    let proofs: Vec<(u64, String)> = recv_msg(&mut rx).await?;
    ensure!(proofs.len() == files.len(), "{name} sent a bad resume list");
    let had: u64 = proofs.iter().map(|(have, _)| have).sum();
    if had > 0 {
        ui.event(Event::Checking { bytes: had });
    }
    let local: Vec<(PathBuf, u64)> = files.iter().map(|(_, path, size)| (path.clone(), *size)).collect();
    let starts = tokio::task::spawn_blocking(move || -> Result<Vec<u64>> {
        local
            .iter()
            .zip(proofs)
            .map(|((path, size), (have, hash))| {
                let same = have > 0 && have <= *size && prefix_hash(path, have)?.to_hex().as_str() == hash;
                Ok(if same { have } else { 0 })
            })
            .collect()
    })
    .await??;
    send_msg(&mut tx, &starts).await?;
    let resumed: u64 = starts.iter().sum();
    let mut progress = Progress::new(ui, total);
    progress.done = resumed;
    let started = Instant::now();
    for ((_, path, size), start) in files.iter().zip(&starts) {
        if start == size {
            continue; // they have all of it
        }
        let mut file = tokio::fs::File::open(path).await?;
        file.seek(std::io::SeekFrom::Start(*start)).await?;
        copy_exact(file, &mut tx, size - start, &mut progress)
            .await
            .with_context(|| format!("sending {}", path.display()))?;
    }
    tx.finish()?;
    ensure!(matches!(rx.read_u8().await, Ok(1)), "{name} didn't confirm the files arrived");
    progress.finish();
    let direct = conn.paths().iter().any(|p| p.is_selected() && p.is_ip());
    // Under 1 MB the time is mostly setup, so a speed would mislead.
    let moved = total - resumed;
    let mb_per_s = (moved >= 1_000_000).then(|| mb(moved) / started.elapsed().as_secs_f64());
    ep.close().await;
    ui.event(Event::Sent { to: name, files: files.len(), direct, mb_per_s, resumed });
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

async fn listen<U: Ui + Send + 'static>(ui: Arc<U>) -> Result<()> {
    let home = home()?;
    let me = load_key(&home)?;
    let downloads = downloads_dir()?;
    let _lock = lock_listener(&home)?;
    let ep = bind(me, &[ALPN]).await?;
    ui.event(Event::Ready(&downloads));
    let turns = Arc::new(Turns::default());
    while let Some(incoming) = ep.accept().await {
        let (ui, turns, home, downloads) = (ui.clone(), turns.clone(), home.clone(), downloads.clone());
        // Each transfer runs on its own: a vanished sender takes 30 s to time out and mustn't block others.
        tokio::spawn(async move {
            if let Err(e) = receive(incoming, &home, &downloads, &*ui, &turns).await {
                ui.event(Event::Failed(format!("{e:#}")));
            }
        });
    }
    Ok(())
}

/// Keeps transfers that run at the same time apart.
#[derive(Default)]
struct Turns {
    ask: tokio::sync::Mutex<()>,                                      // one accept question at a time
    staging: std::sync::Mutex<HashMap<PathBuf, (Arc<tokio::sync::Mutex<()>>, Option<WeakConnectionHandle>)>>, // one writer per staging folder
}

/// Two listeners with one key fight over it: the newest silently gets every transfer. Held until dropped.
fn lock_listener(home: &Path) -> Result<std::fs::File> {
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(home.join("listen.lock"))?;
    ensure!(lock.try_lock().is_ok(), "toss is already receiving on this device");
    Ok(lock)
}

async fn receive(incoming: Incoming, home: &Path, downloads: &Path, ui: &impl Ui, turns: &Turns) -> Result<()> {
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
    let paths = sanitize(&offer.files)?;
    // Files collect in a folder of their own and move into Downloads only once all have arrived.
    let staging = staging_dir(downloads, &from, &offer.files);
    // A resend of the same files means the earlier try is dead or unwanted: end it now rather than
    // waiting out its 30 s timeout, then continue from what it left.
    let writer = {
        let mut map = turns.staging.lock().expect("not poisoned");
        let (writer, latest) = map.entry(staging.clone()).or_default();
        if let Some(earlier) = latest.replace(conn.weak_handle()).and_then(|earlier| earlier.upgrade()) {
            earlier.close(VarInt::from_u32(0), b"replaced by a newer try");
        }
        writer.clone()
    };
    let _writing = writer.lock_owned().await;
    let staged: Vec<PathBuf> = paths.iter().map(|segs| staging.join(segs.iter().collect::<PathBuf>())).collect();
    let have: Vec<u64> = staged.iter().zip(&offer.files).map(|(path, (_, size))| already_have(path, *size)).collect();
    let total = offer.files.iter().try_fold(0u64, |sum, f| sum.checked_add(f.1)).context("file sizes in the offer don't add up")?;
    let needed = total - have.iter().sum::<u64>();
    if let Some(free) = free_space(downloads).filter(|free| *free < needed) {
        tx.write_u8(NO_SPACE).await?;
        bail!("not enough disk space for {name}'s files: {} needed, {} free", human(needed), human(free));
    }

    // Show the cleaned names, never the raw ones: they could hide terminal control codes.
    let shown: Vec<(String, u64)> = paths.iter().zip(&offer.files).map(|(segs, (_, size))| (segs.join("/"), *size)).collect();
    let answer = if contacts[&name].auto_accept {
        ui.event(Event::Receiving { from: &name, files: shown.len() });
        Answer::Yes
    } else {
        let _asking = turns.ask.lock().await;
        ui.accept(&name, &shown).await
    };
    if let Answer::Always = answer {
        update_contacts(home, |c| {
            if let Some(c) = c.get_mut(&name) {
                c.auto_accept = true; // not if they were forgotten while we waited
            }
            Ok(())
        })?;
    }
    if let Answer::No = answer {
        tx.write_u8(DECLINED).await?;
        ui.event(Event::Declined { from: &name });
        return Ok(());
    }
    tx.write_u8(ACCEPTED).await?;
    let had: u64 = have.iter().sum();
    if had > 0 {
        ui.event(Event::Checking { bytes: had });
    }
    // Prove what an earlier try left here, so the sender can send only the rest.
    let proofs = tokio::task::spawn_blocking({
        let (staged, have) = (staged.clone(), have.clone());
        move || -> Result<Vec<(u64, String)>> { staged.iter().zip(have).map(|(path, n)| Ok((n, proof(path, n)?))).collect() }
    })
    .await??;
    send_msg(&mut tx, &proofs).await?;
    let starts: Vec<u64> = recv_msg(&mut rx).await?;
    // Only "from the start" or "from what we have" is valid; anything else would leave holes in files.
    ensure!(
        starts.len() == have.len() && starts.iter().zip(&have).all(|(start, have)| *start == 0 || start == have),
        "{name} sent a bad resume plan"
    );
    let resumed: u64 = starts.iter().sum();
    let mut progress = Progress::new(ui, total);
    progress.done = resumed;
    for ((path, (_, size)), start) in staged.iter().zip(&offer.files).zip(&starts) {
        receive_file(&mut rx, path, *start, *size, &mut progress).await?;
    }
    for (top, name) in free_tops(&paths, downloads, |p| p.exists()) {
        tokio::fs::rename(staging.join(top), downloads.join(name)).await?;
    }
    let _ = tokio::fs::remove_dir_all(&staging).await;
    let _ = tokio::fs::remove_dir(staging.parent().expect("staging is inside .toss-incoming")).await; // only if empty
    tx.write_u8(1).await?;
    tx.finish()?;
    progress.finish();
    ui.event(Event::Received { from: &name, files: offer.files.len(), resumed });
    let _ = tokio::time::timeout(Duration::from_secs(5), conn.closed()).await; // let the confirmation arrive
    Ok(())
}

/// Writes one file into the staging folder, continuing at `start`. Until it's complete it keeps a
/// `.toss-part` name, so it never looks finished and is kept for the next try if the transfer breaks.
async fn receive_file(
    rx: &mut (impl AsyncRead + Unpin),
    staged: &Path,
    start: u64,
    size: u64,
    p: &mut Progress<'_, impl Ui>,
) -> Result<()> {
    if start == size && tokio::fs::try_exists(staged).await? {
        return Ok(()); // finished on an earlier try, and the sender confirmed it's the same
    }
    if let Some(dir) = staged.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let _ = tokio::fs::remove_file(staged).await; // an older finished copy that no longer matches
    let part = part_of(staged);
    let mut file = tokio::fs::OpenOptions::new().write(true).create(true).truncate(false).open(&part).await?;
    file.set_len(start).await?; // drop anything past what the sender confirmed
    file.seek(std::io::SeekFrom::Start(start)).await?;
    copy_exact(&mut *rx, &mut file, size - start, p).await?;
    file.flush().await?;
    drop(file); // Windows can't rename an open file
    tokio::fs::rename(&part, staged).await?;
    Ok(())
}

/// Where an incoming transfer collects until it's complete: one folder per sender and file list, so
/// sending the same files again continues there and different transfers never mix.
// shortcut: abandoned transfers stay in Downloads/.toss-incoming until deleted by hand; add an
// age-based cleanup if they pile up.
fn staging_dir(downloads: &Path, from: &str, files: &[(String, u64)]) -> PathBuf {
    let mut files: Vec<_> = files.iter().collect();
    files.sort(); // picking the same files in another order is still the same transfer
    let mut key = blake3::Hasher::new();
    key.update(from.as_bytes());
    for (rel, size) in files {
        key.update(rel.as_bytes());
        key.update(&[0]);
        key.update(&size.to_le_bytes());
    }
    downloads.join(".toss-incoming").join(&key.finalize().to_hex()[..16])
}

fn part_of(staged: &Path) -> PathBuf {
    let mut part = staged.as_os_str().to_owned();
    part.push(".toss-part");
    part.into()
}

/// Bytes of a file an earlier try left: the whole finished copy, or the unfinished part.
fn already_have(staged: &Path, size: u64) -> u64 {
    let len = |path: &Path| std::fs::metadata(path).map(|m| m.len()).ok();
    match len(staged) {
        Some(n) if n == size => n,
        _ => len(&part_of(staged)).unwrap_or(0).min(size),
    }
}

/// Hash of the first `have` bytes we hold for a file ("" when we hold none).
fn proof(staged: &Path, have: u64) -> Result<String> {
    if have == 0 {
        return Ok(String::new());
    }
    let finished = std::fs::metadata(staged).is_ok_and(|m| m.len() == have);
    Ok(prefix_hash(&if finished { staged.to_path_buf() } else { part_of(staged) }, have)?.to_hex().to_string())
}

/// BLAKE3 of a file's first `len` bytes: lets two devices agree a partial copy is the real thing.
fn prefix_hash(path: &Path, len: u64) -> Result<blake3::Hash> {
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(std::io::Read::take(std::fs::File::open(path)?, len))?;
    Ok(hasher.finalize())
}

/// The sender's paths as plain, safe names. Refuses duplicates and anything that could escape.
fn sanitize(files: &[(String, u64)]) -> Result<Vec<Vec<String>>> {
    let mut seen = HashSet::new();
    files
        .iter()
        .map(|(rel, _)| {
            let segs = rel.split('/').map(safe_name).collect::<Result<Vec<_>>>()?;
            ensure!(seen.insert(segs.join("/").to_lowercase()), "duplicate path in offer: {rel}");
            Ok(segs)
        })
        .collect()
}

/// A free name in `dir` for each distinct top-level file or folder, so nothing existing is overwritten.
fn free_tops<'a>(paths: &'a [Vec<String>], dir: &Path, exists: impl Fn(&Path) -> bool) -> Vec<(&'a str, String)> {
    let mut used = HashSet::new();
    let mut out: Vec<(&str, String)> = Vec::new();
    for top in paths.iter().map(|segs| segs[0].as_str()) {
        if out.iter().any(|(seen, _)| *seen == top) {
            continue;
        }
        let name = free_name(top, |n| exists(&dir.join(n)) || used.contains(&n.to_lowercase()));
        used.insert(name.to_lowercase());
        out.push((top, name));
    }
    out
}

/// Bytes the current user may still write on the disk holding `dir`, if the OS tells us.
#[cfg(windows)]
fn free_space(dir: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    unsafe extern "system" {
        fn GetDiskFreeSpaceExW(dir: *const u16, free_to_caller: *mut u64, total: *mut u64, total_free: *mut u64) -> i32;
    }
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
    let mut free = 0;
    // SAFETY: `wide` is NUL-terminated and outlives the call; null marks the outputs we don't want.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) };
    (ok != 0).then_some(free)
}

#[cfg(unix)]
fn free_space(dir: &Path) -> Option<u64> {
    let path = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: statvfs only fills in `stats`, for which all-zero bytes are a valid value.
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(path.as_ptr(), &mut stats) } == 0).then(|| stats.f_bavail as u64 * stats.f_frsize as u64)
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
    if name.to_lowercase().ends_with(".toss-part") {
        name.push('_'); // our temp-file suffix: "a.toss-part" would be the partial copy of "a"
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
    progress: &mut Progress<'_, impl Ui>,
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

/// Passes progress to the UI at most 4 times a second, plus once at the end.
struct Progress<'a, U> {
    ui: &'a U,
    done: u64,
    total: u64,
    last: Instant,
}

impl<'a, U: Ui> Progress<'a, U> {
    fn new(ui: &'a U, total: u64) -> Self {
        Self { ui, done: 0, total, last: Instant::now() }
    }
    fn add(&mut self, n: u64) {
        self.done += n;
        if self.done < self.total && self.last.elapsed() > Duration::from_millis(250) {
            self.last = Instant::now();
            self.ui.progress(self.done, self.total);
        }
    }
    fn finish(&self) {
        self.ui.progress(self.total, self.total);
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
    // Grow the buffer as bytes arrive: reserving `len` up front would let anyone who connects make us
    // hold 16 MB per connection by sending 4 bytes.
    let mut bytes = Vec::new();
    (&mut *r).take(len as u64).read_to_end(&mut bytes).await?;
    ensure!(bytes.len() == len, "message ended early");
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

/// Accepts the bare link or the whole "toss join toss:… <name>" line someone pasted.
fn parse_link(link: &str) -> Result<(EndpointId, Vec<u8>)> {
    let (id, secret) = link
        .split_whitespace()
        .find_map(|word| word.strip_prefix("toss:"))
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

fn downloads_dir() -> Result<PathBuf> {
    dirs::download_dir().or_else(dirs::home_dir).context("no downloads folder")
}

/// "1 file", "3 files"
fn count(files: usize) -> String {
    if files == 1 { "1 file".into() } else { format!("{files} files") }
}

/// "12 KB", "512.0 MB", "12.4 GB"
fn human(bytes: u64) -> String {
    match bytes {
        1_000_000_000.. => format!("{:.1} GB", bytes as f64 / 1e9),
        1_000_000.. => format!("{:.1} MB", mb(bytes)),
        _ => format!("{:.0} KB", bytes as f64 / 1e3),
    }
}

/// "continued after 12.4 GB" when a transfer picked up where an earlier try stopped.
fn continued(resumed: u64) -> Option<String> {
    (resumed > 0).then(|| format!("continued after {}", human(resumed)))
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / 1e6
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_stay_inside_downloads_and_work_on_every_os() {
        for bad in ["", "..", "a/../b", "/etc/passwd", "a//b", "dir/."] {
            assert!(sanitize(&[(bad.into(), 1)]).is_err(), "{bad:?} should be rejected");
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
            ("a.bin.toss-part", "a.bin.toss-part_"),
            ("A.BIN.TOSS-PART", "A.BIN.TOSS-PART_"),
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
        let paths = sanitize(&files).unwrap();
        let tops = free_tops(&paths, dl, |p| p == dl.join("a.txt") || p == dl.join("dir"));
        let want = [("a.txt", "a (1).txt"), ("a (1).txt", "a (1) (1).txt"), ("dir", "dir (1)")];
        assert_eq!(tops, want.map(|(top, name)| (top, name.to_string())));
        assert!(sanitize(&[("A.txt".into(), 1), ("a.txt".into(), 1)]).is_err());
    }

    #[test]
    fn finds_what_an_earlier_try_left() {
        let dir = std::env::temp_dir().join(format!("toss-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let staged = dir.join("big.bin");
        assert_eq!(already_have(&staged, 10), 0);
        assert_eq!(proof(&staged, 0).unwrap(), "");
        std::fs::write(part_of(&staged), b"hello").unwrap();
        assert_eq!(already_have(&staged, 10), 5);
        assert_eq!(already_have(&staged, 3), 3, "never more than the file's size");
        assert_eq!(proof(&staged, 5).unwrap(), blake3::hash(b"hello").to_hex().to_string());
        std::fs::write(&staged, b"0123456789").unwrap();
        assert_eq!(already_have(&staged, 10), 10, "a finished copy wins over a part");
        assert_eq!(proof(&staged, 10).unwrap(), blake3::hash(b"0123456789").to_hex().to_string());
        assert_eq!(already_have(&staged, 12), 5, "a finished copy of the wrong size doesn't count");
        assert!(free_space(&dir).is_some_and(|free| free > 0), "the OS reports free space");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn resuming_drops_junk_and_keeps_confirmed_files() {
        let dir = std::env::temp_dir().join(format!("toss-resume-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let staged = dir.join("f.bin");
        std::fs::write(part_of(&staged), b"hello WORLD").unwrap(); // the sender confirmed only "hello"
        let mut progress = Progress::new(&Cli, 10);
        receive_file(&mut &b"12345"[..], &staged, 5, 10, &mut progress).await.unwrap();
        assert_eq!(std::fs::read(&staged).unwrap(), b"hello12345");
        assert!(!part_of(&staged).exists(), "the part became the finished file");
        receive_file(&mut &b""[..], &staged, 10, 10, &mut progress).await.unwrap();
        assert_eq!(std::fs::read(&staged).unwrap(), b"hello12345", "a confirmed finished file is left alone");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn same_files_in_any_order_share_a_staging_folder() {
        let dl = Path::new("dl");
        let (a, b) = (("a.iso".to_string(), 1), ("b.iso".to_string(), 2));
        assert_eq!(staging_dir(dl, "me", &[a.clone(), b.clone()]), staging_dir(dl, "me", &[b.clone(), a.clone()]));
        assert_ne!(staging_dir(dl, "me", &[a.clone(), b]), staging_dir(dl, "me", &[a, ("b.iso".into(), 3)]));
    }

    #[tokio::test]
    async fn messages_roundtrip_and_short_ones_fail() {
        let mut wire = Vec::new();
        send_msg(&mut wire, &vec![1u64, 2]).await.unwrap();
        assert_eq!(recv_msg::<Vec<u64>>(&mut &wire[..]).await.unwrap(), [1, 2]);
        let claims_16_mb = (16u32 << 20).to_be_bytes();
        assert!(recv_msg::<Vec<u64>>(&mut &claims_16_mb[..]).await.is_err(), "4 bytes can't pass for 16 MB");
    }

    #[test]
    fn sizes_read_naturally() {
        assert_eq!([human(12_000), human(512_000_000), human(12_400_000_000)], ["12 KB", "512.0 MB", "12.4 GB"]);
    }

    #[test]
    fn invite_link_roundtrip() {
        let id = SecretKey::generate().public();
        let (got_id, got_secret) = parse_link(&format!(" toss:{id}/{} ", hex(&[7; 16]))).unwrap();
        assert_eq!((got_id, got_secret), (id, vec![7; 16]));
        let pasted = format!("toss join toss:{id}/{} <name for you>", hex(&[7; 16]));
        assert_eq!(parse_link(&pasted).unwrap().0, id);
        assert!(parse_link("toss:nope").is_err());
    }
}
