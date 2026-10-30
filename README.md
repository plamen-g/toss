<p align="center"><img src="assets/logo.jpg" width="160" alt="toss logo: a figure throwing a glowing file"></p>

# toss

Send files to your own devices and your friends, peer to peer. No IP addresses to type, no accounts, and your files never sit on a server.

Pair once with an invite link, then toss files by name from any network. Run `toss` to open the window: drag files onto it (or click **Toss**) and pick a device. Closing the window keeps toss receiving from the tray. Right-click the tray icon to start toss at login, use it with Claude Code and, on Windows, add it to Explorer's **Send to** menu. (On GNOME the tray needs the AppIndicator extension.)

The same from the command line:

```
toss invite laptop                  # prints a link; send it to the other device
toss join toss:7de0…/1eab… desktop  # on the other device
toss send laptop report.pdf photos/ # files and folders
toss listen                         # receive (keep it running)
```

Also `toss contacts`, `toss forget <name>`, `toss phone` and `toss relay`. Answer **a**lways to auto-accept a contact.

## Phones, no app

Click **Phone** (or run `toss phone [file]...`) and scan the QR code. The phone's browser opens a page served by your computer: it downloads the files you offer and sends files back into your Downloads folder.

- Same network only, and **not encrypted**: it's plain HTTP, because browsers don't trust certificates a home computer can make. A one-time secret guards the link, the link dies when you leave the page, and only the first browser to open it gets in. Use it at home, not on public Wi-Fi.
- The first time, allow toss through the Windows firewall. On an iPhone, downloads land in Files › Downloads.

## AI assistants (MCP)

`toss mcp` lets an assistant like Claude send files to your devices: "send the chart you just made to my laptop". It can list your paired devices, send to them, put files on your phone ("put this on my phone": the toss window shows a QR code to scan), and see what arrived lately ("open the photo my phone just sent"; toss keeps the names of the last 50 arrivals in its folder for this). Nothing else: pairing, removing devices and settings stay with you, and someone on the other device still accepts each transfer, unless it takes files from yours without asking. The toss window shows what assistants sent.

What an assistant reads can steer it, so toss decides what leaves, not the assistant. Files from the folders you name go at once. For anything else, and for files that look like secrets (keys, `.env`, password databases) even inside those folders, toss asks you itself: in a window of its own that no assistant or app can answer for you, or through the assistant's app on a computer without a screen. The question shows where the files really are and how much would go. Tick **Don't ask again** to let a folder through from then on; the tray lists those folders, and clicking one takes it off. toss's own folder, with your device key, never goes. toss knows secrets by their file names, not their contents, so keep other secrets out of the folders you allow. How this holds up against prompt injection: [evals/injection](evals/injection/README.md).

Claude Code: installed on Windows, toss adds itself (tray menu: **Use with Claude Code**) and asks you before every send. By hand: `claude mcp add toss -- toss mcp ~/projects`. Claude Desktop: download `toss.mcpb` from [Releases](https://github.com/plamen-g/toss/releases), open it and pick the folders. Other assistants (Cursor, ...) take it in their MCP config:

```json
{ "mcpServers": { "toss": { "command": "toss", "args": ["mcp", "/path/to/projects"] } } }
```

Where the assistant supports it, a send runs as a task, so waiting for someone to accept can't time out. Progress shows while files move, and a cancelled send resumes the next time. It's in the [MCP Registry](https://registry.modelcontextprotocol.io) as `io.github.plamen-g/toss`.

## Privacy

Transfers are encrypted end to end (QUIC with TLS 1.3 and a post-quantum X25519 + ML-KEM-768 key exchange): only the receiving device can read your files, or even their names. Others can see *that* you transfer:

| Who | Sees |
|---|---|
| The other device | Your IP address |
| n0, whose free servers toss uses by default to connect devices | Your IP and device ID, which devices connect and when, and the volume through its relay |
| Your internet provider or Wi-Fi | Encrypted traffic and its size |
| Anyone intercepting your Wi-Fi, for the phone page | The files: that page isn't encrypted |

To show less:

- **Run your own relay** ([below](#use-your-own-relay)) and toss never contacts n0. Without one, even devices on the same Wi-Fi use n0 to find each other.
- **A removed device still knows your device ID**, so it can see when you're online. Delete the `key` file in toss's config folder for a new identity, then pair again.
- **An invite link works like a ticket**: whoever uses it first is paired. Send links over a channel you trust.
- **macOS and Linux** don't mark received files as downloaded, so the window offers Show in folder there, not Open.

## Install

On Windows, download `toss-windows-x86_64.exe` from [Releases](https://github.com/plamen-g/toss/releases) and open it: toss installs itself, with a Start menu entry, starting at login, and Claude Code if you have it. Opening a newer download updates it. Builds aren't signed yet, so Windows asks first: click "More info" → "Run anyway".

On macOS and Linux, put `toss` from Releases on your PATH (on macOS, run `xattr -d com.apple.quarantine toss` first). Or build it: `cargo build --release` (about 14 MB).

Keys and contacts live in `%APPDATA%\toss`, `~/.config/toss` or `~/Library/Application Support/toss`, or wherever `TOSS_HOME` points.

## Use your own relay

Run [`iroh-relay`](https://github.com/n0-computer/iroh) on a server with a hostname (`iroh-relay --config-path relay.toml`, open TCP 80 and 443 and UDP 7842):

```toml
enable_relay = true
enable_quic_addr_discovery = true
access.shared_token = ["<long random token>"]  # only your devices may use it

[tls]
hostname = ["relay.example.com"]
cert_mode = "LetsEncrypt"
contact = "it@example.com"
```

Then on every device: `toss relay https://relay.example.com <token>`, or deploy the `relay.json` it writes. `toss relay off` undoes it. Devices only reach each other through the same relay.

## Always-on receiver

Pair a server once, answer **a**lways, and keep `toss listen` running; with nobody to answer, only auto-accept contacts get through.

```ini
# /etc/systemd/system/toss.service, then: systemctl enable --now toss
[Unit]
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/toss listen
Restart=on-failure
StandardInput=null

[Install]
WantedBy=multi-user.target
```

## How it works

- Built on [iroh](https://iroh.computer): a device is its public key. Connections go straight through NATs when they can, otherwise through an encrypted relay.
- Every connection opens with a signature over its own TLS keys, so a contact can't be faked or replayed. Invite links hold a throwaway key and a 128-bit secret.
- Received files can't land outside Downloads, never overwrite anything (`name (1).ext`), get names that work on every OS, and on Windows carry the "downloaded from the internet" mark. Strangers are turned away, and free space is checked first.
- Big files stream in 256 KB pieces, so memory stays around 10 MB however large they are. A broken transfer resumes when you send the same files again, after both sides compare a BLAKE3 hash of what already arrived. Partials wait in `Downloads/.toss-incoming`; delete abandoned ones by hand.

## Not yet

Nearby discovery, a check code after pairing, the macOS download flag, drag and drop on Wayland.

MIT licensed. Display font: [Bricolage Grotesque](https://github.com/ateliertriay/bricolage) (SIL Open Font License, `assets/OFL-BricolageGrotesque.txt`).
