# toss

Send files to your own devices and your friends, peer to peer. No IP addresses, no accounts, no cloud.

You pair with someone once by sending them an invite link. After that you toss files to them by name, from any network.

```
toss invite laptop                  # prints a link; send it to the other device
toss join toss:7de0…/1eab… desktop  # on the other device
toss listen                         # on the receiving side, keep it running
toss send laptop report.pdf photos/ # files and folders
```

Other commands: `toss contacts` and `toss forget <name>`. When a transfer arrives you can answer **a**lways to auto-accept that contact from then on.

## Build

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
  - File names are made valid for your OS, for example `10:30.log` becomes `10_30.log` and `con.txt` becomes `con_.txt`.
  - Existing files are never overwritten; a new copy becomes `name (1).ext`.
  - Unfinished files keep a `.toss-part` name and are removed when a transfer fails. A killed `toss` can leave one behind.

Keys and contacts live in your config folder (`%APPDATA%\toss`, `~/.config/toss`, `~/Library/Application Support/toss`).

## Not yet

- A tray app with a window. The command line comes first.
- Resuming interrupted transfers.
- Nearby discovery on the same Wi-Fi.
- An MCP server.

Uses n0's free public relays, which are meant for hobby use.

MIT licensed.
