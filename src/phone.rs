//! The phone page: toss serves a small web page on the home network, so a phone can download and
//! upload files with its browser, no app needed. Browsers distrust certificates we could make
//! ourselves, so it's plain HTTP: a one-time secret in the link keeps others out, and the server only
//! runs while the page is open.

use std::{
    io::IsTerminal as _,
    net::{IpAddr, TcpListener},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use iroh::SecretKey;
use qrcodegen::{QrCode, QrCodeEcc};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use crate::{Event, Progress, Ui, copy_exact, free_name, free_space, hex, human, mark_downloaded, mb, part_of, safe_name, same};

/// A running phone page. Dropping it stops the server; transfers already underway finish.
pub struct Share {
    pub url: String,
    state: Arc<State>,
    server: tokio::task::AbortHandle,
}

struct State {
    secret: String,
    downloads: PathBuf,
    offered: Mutex<Vec<PathBuf>>,
    uploading: tokio::sync::Mutex<()>, // one at a time, so two uploads can't pick the same free name
    received: Mutex<Vec<PathBuf>>,     // everything this page has received, so a batch reads as one
    phone: Mutex<Option<IpAddr>>,      // the first device to use the link; anyone else holding it is turned away
}

impl Share {
    /// Files the phone can download, replacing earlier ones.
    pub fn offer(&self, files: Vec<PathBuf>) {
        *self.state.offered.lock().expect("not poisoned") = files;
    }
}

impl Drop for Share {
    fn drop(&mut self) {
        self.server.abort();
    }
}

pub fn start<U: Ui + Send + 'static>(ui: Arc<U>) -> Result<Share> {
    let listener = TcpListener::bind((lan_ip()?, 0))?;
    listener.set_nonblocking(true)?;
    let listener = tokio::net::TcpListener::from_std(listener)?;
    let secret = hex(&SecretKey::generate().to_bytes()[..16]); // 128 bits from the OS random generator
    let url = format!("http://{}/{secret}", listener.local_addr()?);
    let state = Arc::new(State {
        secret,
        downloads: crate::downloads_dir()?,
        offered: Mutex::default(),
        uploading: tokio::sync::Mutex::default(),
        received: Mutex::default(),
        phone: Mutex::default(),
    });
    let server = tokio::spawn({
        let state = state.clone();
        async move {
            while let Ok((mut stream, peer)) = listener.accept().await {
                let (state, ui) = (state.clone(), ui.clone());
                tokio::spawn(async move {
                    let (r, mut w) = stream.split();
                    serve(&mut BufReader::new(r), &mut w, &state, &*ui, peer.ip()).await;
                    let _ = w.shutdown().await;
                });
            }
        }
    })
    .abort_handle();
    Ok(Share { url, state, server })
}

/// `toss phone [file]...`: the phone page from the terminal, until Ctrl+C.
pub async fn cli(files: &[&str]) -> Result<()> {
    let files: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
    for file in &files {
        ensure!(file.is_file(), "{} isn't a file (the phone page offers files, not folders)", file.display());
    }
    let share = start(Arc::new(crate::Cli))?;
    share.offer(files);
    println!("Scan with the phone camera, or open {}", share.url);
    println!("Home network only: the page isn't encrypted. Ctrl+C stops it.\n");
    if std::io::stdout().is_terminal() {
        for rows in qr(&share.url).chunks(2) {
            // White on black whatever the terminal's colors: light modules are drawn, dark ones left out.
            let line: String = (0..rows[0].len())
                .map(|x| match (rows[0][x], rows.get(1).is_some_and(|row| row[x])) {
                    (false, false) => '█',
                    (false, true) => '▀',
                    (true, false) => '▄',
                    (true, true) => ' ',
                })
                .collect();
            println!("  \x1b[97;40m{line}\x1b[0m");
        }
    }
    std::future::pending::<()>().await;
    Ok(())
}

/// The link as a QR code: rows of modules (true = dark), with the light border scanners need.
pub fn qr(url: &str) -> Vec<Vec<bool>> {
    let code = QrCode::encode_text(url, QrCodeEcc::Medium).expect("a link this short always fits");
    let n = code.size();
    (-4..n + 4).map(|y| (-4..n + 4).map(|x| code.get_module(x, y)).collect()).collect()
}

/// This computer's address on the network its internet traffic goes out on.
// shortcut: with a VPN on, that's the VPN's address and the phone can't reach it; offer every
// address if that comes up.
fn lan_ip() -> Result<IpAddr> {
    let probe = std::net::UdpSocket::bind("0.0.0.0:0")?;
    probe.connect("192.0.2.1:9").context("not connected to a network")?; // only picks a route, sends nothing
    Ok(probe.local_addr()?.ip())
}

/// Answers one request, then the connection closes: browsers cope, and it keeps this parser small.
async fn serve(
    r: &mut (impl AsyncBufRead + Unpin),
    w: &mut (impl AsyncWrite + Unpin),
    state: &State,
    ui: &impl Ui,
    peer: IpAddr,
) {
    // Anyone on the network can connect: give up on silent or oversized requests.
    let Ok(Ok((method, target, len))) = tokio::time::timeout(Duration::from_secs(10), request(r)).await else { return };
    let mut parts = target.split('?').next().unwrap_or_default().trim_start_matches('/').splitn(3, '/');
    let res = if !parts.next().is_some_and(|s| same(s.as_bytes(), state.secret.as_bytes())) {
        reply(w, "404 Not Found", TEXT, b"Not found").await
    } else if !claim(state, peer, ui) {
        let text = b"This page is already open on another device. Open Phone again on the computer for a new code.";
        reply(w, "403 Forbidden", TEXT, text).await
    } else {
        match (method.as_str(), parts.next(), parts.next()) {
            ("GET", None | Some(""), None) => reply(w, "200 OK", "text/html; charset=utf-8", PAGE.as_bytes()).await,
            ("GET", Some("list"), None) => list(w, state).await,
            ("GET", Some("get"), Some(which)) => download(w, state, which, ui).await,
            ("PUT", Some("put"), Some(name)) => upload(r, w, state, name, len, ui).await,
            _ => reply(w, "404 Not Found", TEXT, b"Not found").await,
        }
    };
    if let Err(e) = res {
        ui.event(Event::Failed(format!("phone: {e:#}")));
    }
}

/// The first device to use the link gets the page. Anyone else holding it, from a sniffed or
/// photographed code, is turned away.
fn claim(state: &State, peer: IpAddr, ui: &impl Ui) -> bool {
    let (first, new) = {
        let mut phone = state.phone.lock().expect("not poisoned");
        let new = phone.is_none();
        (*phone.get_or_insert(peer), new)
    };
    if new {
        ui.event(Event::PhoneConnected(peer));
    }
    first == peer
}

/// The method, target and Content-Length of a request. Refuses heads over 8 KB.
async fn request(r: &mut (impl AsyncBufRead + Unpin)) -> Result<(String, String, Option<u64>)> {
    let mut lines = Vec::new();
    let mut size = 0;
    loop {
        let mut line = String::new();
        size += (&mut *r).take(8192 - size as u64).read_line(&mut line).await?;
        ensure!(line.ends_with('\n'), "request head too long or cut off");
        if line.trim_end().is_empty() {
            break;
        }
        lines.push(line.trim_end().to_string());
    }
    let mut first = lines.first().context("empty request")?.split(' ');
    let (method, target) = (first.next().unwrap_or_default().to_string(), first.next().context("no target")?.to_string());
    let mut len = None;
    for line in &lines[1..] {
        let (key, value) = line.split_once(':').context("bad header")?;
        match key.trim().to_ascii_lowercase().as_str() {
            "content-length" => len = Some(value.trim().parse()?),
            "transfer-encoding" => bail!("chunked uploads aren't supported"), // browsers send a length for files
            _ => {}
        }
    }
    Ok((method, target, len))
}

async fn head(w: &mut (impl AsyncWrite + Unpin), status: &str, kind: &str, len: u64, extra: &str) -> Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {len}\r\n{extra}Cache-Control: no-store\r\nConnection: close\r\n\r\n"
    );
    Ok(w.write_all(head.as_bytes()).await?)
}

const TEXT: &str = "text/plain; charset=utf-8";

async fn reply(w: &mut (impl AsyncWrite + Unpin), status: &str, kind: &str, body: &[u8]) -> Result<()> {
    head(w, status, kind, body.len() as u64, "").await?;
    Ok(w.write_all(body).await?)
}

/// The offered files as JSON: `[{"name": "a.pdf", "size": 1234}]`.
async fn list(w: &mut (impl AsyncWrite + Unpin), state: &State) -> Result<()> {
    let files: Vec<serde_json::Value> = state
        .offered
        .lock()
        .expect("not poisoned")
        .iter()
        .map(|path| {
            let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
            serde_json::json!({ "name": name, "size": std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) })
        })
        .collect();
    reply(w, "200 OK", "application/json", &serde_json::to_vec(&files)?).await
}

/// Streams one offered file. `which` is "<index>/<name>", so a link from a list that has changed
/// since can't fetch a different file.
async fn download(w: &mut (impl AsyncWrite + Unpin), state: &State, which: &str, ui: &impl Ui) -> Result<()> {
    let path = which.split_once('/').and_then(|(i, name)| {
        let path = state.offered.lock().expect("not poisoned").get(i.parse::<usize>().ok()?)?.clone();
        (path.file_name()?.to_string_lossy() == pct_decode(name)?).then_some(path)
    });
    let Some(path) = path else { return reply(w, "404 Not Found", TEXT, b"No longer offered").await };
    let mut file = tokio::fs::File::open(&path).await?;
    let size = file.metadata().await?.len();
    let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    let disposition = format!("Content-Disposition: attachment; filename*=UTF-8''{}\r\n", pct_encode(&name));
    head(w, "200 OK", "application/octet-stream", size, &disposition).await?;
    ui.event(Event::Sending { to: "phone", files: 1 });
    let mut progress = Progress::new(ui, size);
    let started = Instant::now();
    copy_exact(&mut file, w, size, &mut progress).await.with_context(|| format!("sending {name}"))?;
    progress.finish();
    let mb_per_s = (size >= 1_000_000).then(|| mb(size) / started.elapsed().as_secs_f64());
    ui.event(Event::Sent { to: "phone", files: 1, direct: true, mb_per_s, resumed: 0 });
    Ok(())
}

/// Saves one file from the phone in Downloads, under a name that doesn't overwrite anything.
async fn upload(
    r: &mut (impl AsyncRead + Unpin),
    w: &mut (impl AsyncWrite + Unpin),
    state: &State,
    name: &str,
    len: Option<u64>,
    ui: &impl Ui,
) -> Result<()> {
    let (Some(name), Some(len)) = (pct_decode(name).and_then(|n| safe_name(&n).ok()), len) else {
        return reply(w, "400 Bad Request", TEXT, b"Bad request").await;
    };
    if let Some(free) = free_space(&state.downloads).filter(|free| *free < len) {
        reply(w, "507 Insufficient Storage", TEXT, b"Not enough disk space on the computer").await?;
        bail!("not enough disk space for {name}: {} needed, {} free", human(len), human(free));
    }
    let _one = state.uploading.lock().await;
    let path = state.downloads.join(free_name(&name, |n| state.downloads.join(n).exists()));
    let part = part_of(&path); // never looks finished until it is
    ui.event(Event::Receiving { from: "phone", files: 1 });
    let mut progress = Progress::new(ui, len);
    let saved: Result<()> = async {
        let mut file = tokio::fs::File::create(&part).await?;
        copy_exact(r, &mut file, len, &mut progress).await?;
        file.flush().await?;
        drop(file); // Windows can't rename an open file
        Ok(tokio::fs::rename(&part, &path).await?)
    }
    .await;
    if saved.is_err() {
        let _ = tokio::fs::remove_file(&part).await;
    }
    saved.with_context(|| format!("receiving {name}"))?;
    mark_downloaded(&path);
    progress.finish();
    // A phone sends a batch one file at a time: report the whole batch so far.
    let received = {
        let mut received = state.received.lock().expect("not poisoned");
        received.push(path);
        received.clone()
    };
    ui.event(Event::Received { from: "phone", files: received.len(), resumed: 0, saved: &received });
    reply(w, "200 OK", TEXT, b"OK").await
}

/// "%C3%A9t%C3%A9.jpg" -> "été.jpg"; None for broken escapes or text that isn't UTF-8.
fn pct_decode(s: &str) -> Option<String> {
    let mut out = Vec::new();
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        out.push(if b == b'%' { u8::from_str_radix(std::str::from_utf8(&[bytes.next()?, bytes.next()?]).ok()?, 16).ok()? } else { b });
    }
    String::from_utf8(out).ok()
}

/// Escapes everything but letters, digits and `-._~`, for a header value.
fn pct_encode(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

const PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>toss</title>
<style>
  :root { color-scheme: dark; }
  body { margin: 0; padding: 20px; background: #0c1a37; color: #eaf2ff; font: 16px -apple-system, system-ui, sans-serif; }
  h1 { font-size: 24px; margin: 4px 0 18px; }
  section { background: #ffffff0f; border: 1px solid #ffffff1c; border-radius: 16px; padding: 16px; margin-bottom: 14px; }
  h2 { font-size: 13px; font-weight: 600; color: #93a6c9; margin: 0 0 8px; }
  a { display: flex; gap: 12px; justify-content: space-between; padding: 12px 0; color: #22d3ee; text-decoration: none; border-top: 1px solid #ffffff1c; overflow-wrap: anywhere; }
  a:first-child { border-top: 0; }
  .muted { color: #93a6c9; font-size: 14px; }
  button { width: 100%; padding: 14px; border: 0; border-radius: 999px; background: #22d3ee; color: #062a33; font: 600 16px -apple-system, system-ui, sans-serif; }
</style>
</head>
<body>
<h1>toss</h1>
<section><h2>From the computer</h2><div id="offered" class="muted">None</div></section>
<section><h2>To the computer</h2><input id="pick" type="file" multiple hidden><button id="choose">Choose files</button><p id="note" class="muted"></p></section>
<script>
const base = location.pathname.replace(/\/$/, '');
const $ = id => document.getElementById(id);
const size = b => b >= 1e9 ? (b / 1e9).toFixed(1) + ' GB' : b >= 1e6 ? (b / 1e6).toFixed(1) + ' MB' : Math.round(b / 1e3) + ' KB';
let shown = '';
async function refresh() {
  try {
    const text = await (await fetch(base + '/list')).text();
    if (text === shown) return;
    shown = text;
    const files = JSON.parse(text);
    $('offered').replaceChildren(...files.map((f, i) => {
      const a = document.createElement('a');
      a.href = base + '/get/' + i + '/' + encodeURIComponent(f.name);
      a.textContent = f.name;
      const s = document.createElement('span');
      s.className = 'muted';
      s.textContent = size(f.size);
      a.append(s);
      return a;
    }));
    if (!files.length) $('offered').textContent = 'None';
  } catch {
    shown = '';
    $('offered').textContent = 'Not connected. The page on the computer may be closed.';
  }
}
function put(file) {
  return new Promise(done => {
    const x = new XMLHttpRequest();
    x.open('PUT', base + '/put/' + encodeURIComponent(file.name));
    x.upload.onprogress = e => { if (e.lengthComputable) $('note').textContent = file.name + ' · ' + Math.round(100 * e.loaded / e.total) + '%'; };
    x.onload = () => done(x.status === 200 ? '' : x.responseText || 'failed');
    x.onerror = () => done('connection lost');
    x.send(file);
  });
}
$('choose').onclick = () => $('pick').click();
$('pick').onchange = async () => {
  const files = [...$('pick').files];
  $('pick').value = '';
  for (const f of files) {
    const err = await put(f);
    if (err) { $('note').textContent = f.name + ': ' + err; return; }
  }
  $('note').textContent = 'Sent ' + files.length + (files.length === 1 ? ' file' : ' files');
};
refresh();
setInterval(refresh, 3000);
</script>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_survive_the_url() {
        for name in ["IMG_0001.HEIC", "été 2026 (1).jpg", "a/b?c#d%e.txt", "日本.pdf"] {
            assert_eq!(pct_decode(&pct_encode(name)).as_deref(), Some(name));
        }
        assert_eq!(pct_decode("a%2"), None);
        assert_eq!(pct_decode("a%zz"), None);
        assert_eq!(pct_decode("%FF"), None, "not UTF-8");
    }

    #[tokio::test]
    async fn reads_a_request_head_and_refuses_junk() {
        let req = b"PUT /s/put/a.jpg HTTP/1.1\r\nHost: x\r\nContent-Length: 12\r\n\r\nbody";
        let (method, target, len) = request(&mut &req[..]).await.unwrap();
        assert_eq!((method.as_str(), target.as_str(), len), ("PUT", "/s/put/a.jpg", Some(12)));
        assert!(request(&mut &b"GET / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n"[..]).await.is_err());
        assert!(request(&mut &b"GET / HTTP/1.1\r\nHost: x"[..]).await.is_err(), "cut off");
        let huge = format!("GET / HTTP/1.1\r\nX: {}\r\n\r\n", "a".repeat(9000));
        assert!(request(&mut huge.as_bytes()).await.is_err(), "over 8 KB");
    }

    #[tokio::test]
    async fn uploads_stay_in_downloads_and_never_overwrite() {
        let dir = std::env::temp_dir().join(format!("toss-phone-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = State {
            secret: "s3cret".into(),
            downloads: dir.clone(),
            offered: Mutex::default(),
            uploading: tokio::sync::Mutex::default(),
            received: Mutex::default(),
            phone: Mutex::default(),
        };
        let (phone, other): (IpAddr, IpAddr) = ([192, 168, 1, 30].into(), [192, 168, 1, 31].into());
        let put = |secret: &str| format!("PUT /{secret}/put/..%2F..%2Fx.bin HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello");
        for _ in 0..2 {
            let (request, mut response) = (put("s3cret"), Vec::new());
            serve(&mut request.as_bytes(), &mut response, &state, &crate::Cli, phone).await;
            assert!(response.starts_with(b"HTTP/1.1 200"), "{}", String::from_utf8_lossy(&response));
        }
        assert_eq!(std::fs::read(dir.join(".._.._x.bin")).unwrap(), b"hello", "the name can't climb out");
        assert_eq!(std::fs::read(dir.join(".._.._x (1).bin")).unwrap(), b"hello", "the second copy gets a new name");
        let (request, mut response) = (put("wrong"), Vec::new());
        serve(&mut request.as_bytes(), &mut response, &state, &crate::Cli, phone).await;
        assert!(response.starts_with(b"HTTP/1.1 404"));
        let (request, mut response) = (put("s3cret"), Vec::new());
        serve(&mut request.as_bytes(), &mut response, &state, &crate::Cli, other).await;
        assert!(response.starts_with(b"HTTP/1.1 403"), "the link belongs to the first phone");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2, "nothing else was written");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn qr_has_a_quiet_border() {
        let rows = qr("http://192.168.1.20:52814/00112233445566778899aabbccddeeff");
        assert_eq!(rows.len(), rows[0].len());
        assert!(rows[..4].iter().all(|row| row.iter().all(|dark| !dark)));
        assert!(rows[4][4], "finder pattern corner");
    }
}
