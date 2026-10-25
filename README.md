<p align="center"><img src="assets/logo.jpg" width="160" alt="toss logo: a figure throwing a glowing file"></p>

# toss

Send files to your own devices and your friends, peer to peer. No IP addresses to type, no accounts, and your files never sit on a server.

You pair with someone once by sending them an invite link. After that you toss files to them by name, from any network.

Run `toss` with no arguments to open the window. Drag files onto it (or click **Toss**) and pick a device. Closing it keeps toss running in the system tray so it can still receive. When something arrives the window pops back up. Quit from the tray icon's menu. (On GNOME the tray needs the "AppIndicator and KStatusNotifierItem Support" extension.)

Everything is also on the command line:

```
toss invite laptop                  # prints a link; send it to the other device
toss join toss:7de0…/1eab… desktop  # on the other device
toss listen                         # on the receiving side, keep it running
toss send laptop report.pdf photos/ # files and folders
```

Other commands: `toss contacts` and `toss forget <name>`. When a transfer arrives you can answer **a**lways to auto-accept that contact from then on.

## Phones, no app

Click **Phone** in the window (or run `toss phone [file]...`) and scan the QR code with the phone's camera. The phone's browser opens a page served by your computer, with nothing in between: it downloads the files you chose, and can send photos and files back into your Downloads folder.

- The phone has to be on a network that can reach the computer, like the same home Wi-Fi.
- The page isn't encrypted. Browsers only trust certificates a home computer can't get, so it's plain HTTP. A one-time secret in the link keeps others out, and the link stops working when you leave the page. Use it at home, not on public Wi-Fi.
- The page belongs to the first device that opens it. Anyone else using the same link, say from a photo of the QR code, is turned away, and the window shows which device connected.
- The first time, Windows asks whether toss may accept connections. Allow it for private networks.
- On an iPhone, downloads land in Files › Downloads. From there, "Save to Photos" puts pictures and videos in Photos.

## Privacy: what others can see

Every transfer is encrypted end to end (QUIC with TLS 1.3), with a post-quantum key exchange (X25519 + ML-KEM-768), so traffic recorded today can't be decrypted by a quantum computer later. Only the device you send to can read your files, or even their names. What others can see is *that* you transfer, with whom and when:

- **The other device** learns your IP address, because devices connect directly.
- **n0**, the company behind iroh, runs the free servers toss uses by default to help devices find each other. It sees your IP address and device ID, which devices connect to each other and when, and how much passes through its relay when no direct path works. Your device tells n0's lookup service which relay it uses every 5 minutes.
- **Your internet provider or Wi-Fi network** sees encrypted traffic: that you transfer something, and how much.
- **The phone page** is the exception: it isn't encrypted, so someone who can intercept traffic on your Wi-Fi could see the page's link and the files.

The limits, and what to do about them:

- **n0 sees who talks to whom.** [Run your own relay](#use-your-own-relay): with one set, toss doesn't contact n0 at all.
- **Even on the same Wi-Fi**, where files go straight from one device to the other, n0 helps them find each other, so it learns that they connected (not what, and almost nothing about how much). A relay you run at home, on a Raspberry Pi or a NAS, takes n0 out of that too.
- **Removing a device doesn't hide you from it.** It still knows your device ID, so it can see which relay region you use and when you're online, and it can knock (toss turns it away). To cut it off completely, delete the `key` file in toss's config folder: toss makes a new identity, and you pair your other devices again.
- **An invite link works like a ticket:** whoever uses it first is paired with you. Send links over a channel you trust (in person, or a private messenger), and check your device list afterwards.
- **The phone page** is for your home network, not public Wi-Fi.
- **On macOS and Linux**, received files don't get the "downloaded from the internet" mark Windows uses (see below), so the window offers Show in folder but not Open there. Open received files with the care you'd give an email attachment.

## Install

Download the file for your system from [Releases](https://github.com/plamen-g/toss/releases) and put `toss` somewhere on your PATH. The builds aren't signed yet:
- **Windows:** SmartScreen may warn once. Click "More info" → "Run anyway".
- **macOS:** clear the download flag with `xattr -d com.apple.quarantine toss`.

Or build it yourself:

```
cargo build --release   # target/release/toss, ~13 MB
```

Run two separate identities on one machine by giving each its own `TOSS_HOME` folder.

## Always-on receiver (server / VPS)

Pair the server with your PC, answer **a**lways once, then keep it listening with systemd. With no one there to answer prompts, only auto-accept contacts get through.

```ini
# /etc/systemd/system/toss.service   then: systemctl enable --now toss
[Unit]
Description=toss receiver
After=network-online.target
Wants=network-online.target

[Service]
User=root
ExecStart=/usr/local/bin/toss listen
Restart=on-failure
StandardInput=null

[Install]
WantedBy=multi-user.target
```

Tested between a home PC (Windows) and a Hetzner ARM VPS (Helsinki): direct connection both ways, ~20 MB/s (the home line's limit).

## How it works

- **Connections:** built on [iroh](https://iroh.computer). A device is its public key. iroh finds the device, connects directly through NATs when it can, and otherwise falls back to an encrypted relay. All traffic is QUIC with TLS 1.3, end to end, with the same hybrid post-quantum key exchange browsers use (X25519 + ML-KEM-768).
- **Invites:** an invite link holds a throwaway key and a 128-bit secret. Leaking a used or expired link reveals nothing about your real device id. Relays see the throwaway key, but only someone holding the secret can pair.
- **Identity:** every connection opens with a signature over that connection's TLS key material. A contact's identity can't be faked or replayed, even when the connection itself is made from a throwaway key.
- **Receiving:**
  - Transfers from devices that aren't contacts are dropped.
  - Paths from the sender are checked so nothing can land outside your Downloads folder.
  - File names are made valid for your OS, for example `10:30.log` becomes `10_30.log` and `con.txt` becomes `con_.txt`. Names ending in toss's own `.toss-part` get a `_` added, so they can't clash with an unfinished file. Invisible characters that can make a name display backwards (so `photo.exe` poses as a `.jpg`) are replaced too.
  - On Windows, received files carry the same "downloaded from the internet" mark as browser downloads, so Windows warns before running a program and Office opens documents read-only.
  - Existing files are never overwritten; a new copy becomes `name (1).ext`.
  - Before asking you, toss checks there's enough free disk space; if not, the sender is told so.
- **Big files:** files stream in 256 KB pieces, so memory stays around 10 MB however large they are.
  - Incoming files collect in `Downloads/.toss-incoming` and only move into Downloads once everything has arrived.
  - If a transfer breaks, send the same files again: toss continues where it stopped.
  - Before continuing, both sides compare a BLAKE3 hash of the part already received, so a changed source file or a damaged partial starts over instead of producing a mixed-up file.
  - Abandoned partial transfers stay in `.toss-incoming` until you delete them.

Keys and contacts live in your config folder (`%APPDATA%\toss`, `~/.config/toss`, `~/Library/Application Support/toss`).

## Use your own relay

Most transfers go directly between devices. A relay only helps them find each other, and carries the
(still encrypted) data when a direct path is impossible. By default toss uses n0's free public relays,
which are meant for personal use. Running your own, at a business or at home, keeps even "who talked to
whom" in-house:

1. Run [`iroh-relay`](https://github.com/n0-computer/iroh) on a small server with a hostname, using
   `iroh-relay --config-path relay.toml`:
   ```toml
   enable_relay = true
   enable_quic_addr_discovery = true
   access.shared_token = ["<long random token>"]  # only your devices may use it

   [tls]
   hostname = ["relay.example.com"]
   cert_mode = "LetsEncrypt"
   contact = "it@example.com"
   prod_tls = true
   ```
   Open TCP 80 and 443 and UDP 7842.
2. On every device: `toss relay https://relay.example.com <token>`. It's stored in `relay.json` in
   toss's config folder, so IT can also deploy that file. `toss relay off` goes back. A token needs
   an `https://` address, so it never crosses the network readable.

Devices can only reach each other through the same relay, so set it on all of them.

## Not yet

- Nearby discovery on the same Wi-Fi.
- Drag and drop on Linux under Wayland (it works on Windows, macOS and X11).
- A short code to compare on both screens after pairing, against an intercepted invite link.
- The macOS "downloaded from the internet" flag on received files.
- An MCP server.

MIT licensed. The display font is [Bricolage Grotesque](https://github.com/ateliertriay/bricolage), under the SIL Open Font License (`assets/OFL-BricolageGrotesque.txt`).
