<p align="center"><img src="assets/logo.jpg" width="160" alt="toss logo: a figure throwing a glowing file"></p>

# toss

Send files to your own devices and your friends, peer to peer. No IP addresses, no accounts, no cloud.

You pair with someone once by sending them an invite link. After that you toss files to them by name, from any network.

Run `toss` with no arguments to open the window. Closing it keeps toss running in the system tray so it can still receive. When something arrives the window pops back up. Quit from the tray icon's menu. (On GNOME the tray needs the "AppIndicator and KStatusNotifierItem Support" extension.)

Everything is also on the command line:

```
toss invite laptop                  # prints a link; send it to the other device
toss join toss:7de0…/1eab… desktop  # on the other device
toss listen                         # on the receiving side, keep it running
toss send laptop report.pdf photos/ # files and folders
```

Other commands: `toss contacts` and `toss forget <name>`. When a transfer arrives you can answer **a**lways to auto-accept that contact from then on.

## Install

Download the file for your system from [Releases](https://github.com/plamen-g/toss/releases) and put `toss` somewhere on your PATH. The builds aren't signed yet:
- **Windows:** SmartScreen may warn once. Click "More info" → "Run anyway".
- **macOS:** clear the download flag with `xattr -d com.apple.quarantine toss`.

Or build it yourself:

```
cargo build --release   # target/release/toss, ~5 MB
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

- **Connections:** built on [iroh](https://iroh.computer). A device is its public key. iroh finds the device, connects directly through NATs when it can, and otherwise falls back to an encrypted relay. All traffic is QUIC with TLS 1.3, end to end.
- **Invites:** an invite link holds a throwaway key and a 128-bit secret. Leaking a used or expired link reveals nothing about your real device id. Relays see the throwaway key, but only someone holding the secret can pair.
- **Identity:** every connection opens with a signature over that connection's TLS key material. A contact's identity can't be faked or replayed, even when the connection itself is made from a throwaway key.
- **Receiving:**
  - Transfers from devices that aren't contacts are dropped.
  - Paths from the sender are checked so nothing can land outside your Downloads folder.
  - File names are made valid for your OS, for example `10:30.log` becomes `10_30.log` and `con.txt` becomes `con_.txt`. Names ending in toss's own `.toss-part` get a `_` added, so they can't clash with an unfinished file.
  - Existing files are never overwritten; a new copy becomes `name (1).ext`.
  - Before asking you, toss checks there's enough free disk space; if not, the sender is told so.
- **Big files:** files stream in 256 KB pieces, so memory stays around 10 MB however large they are.
  - Incoming files collect in `Downloads/.toss-incoming` and only move into Downloads once everything has arrived.
  - If a transfer breaks, send the same files again: toss continues where it stopped.
  - Before continuing, both sides compare a BLAKE3 hash of the part already received, so a changed source file or a damaged partial starts over instead of producing a mixed-up file.
  - Abandoned partial transfers stay in `.toss-incoming` until you delete them.

Keys and contacts live in your config folder (`%APPDATA%\toss`, `~/.config/toss`, `~/Library/Application Support/toss`).

## Not yet

- Nearby discovery on the same Wi-Fi.
- An MCP server.

Uses n0's free public relays, which are meant for hobby use.

MIT licensed.
