# steamboat

Copy files between machines on the same LAN. Built for pushing emulation
content (ROMs, BIOS files, saves) from a Mac or Windows desktop onto a Steam
Machine running SteamOS, where ext4-with-casefolding microSD cards make direct
filesystem access from other platforms impossible — but general-purpose: any
files, any direction the roadmap grows into.

One static binary per platform (macOS, Windows, Linux), no runtime
dependencies — it survives SteamOS updates from a home directory.

```
steamboat receive <dest-dir>   # on the machine receiving files
steamboat send <paths...>      # on the machine sending them
```

Peers find each other over mDNS; `--to <ip:port>` works when multicast
doesn't. The receiver confirms every incoming transfer before anything is
written, and retrying an interrupted transfer only re-sends what's missing.

Design: `docs/superpowers/specs/2026-08-16-steamboat-v1-design.md`

## Troubleshooting

- **Windows Firewall** prompts the first time `steamboat receive` binds a
  port — allow it on private networks or the sender can't connect.
- **Discovery finds nothing?** mDNS needs UDP 5353 multicast; VPN clients and
  some routers silently block it. The receiver prints a direct address — use
  `steamboat send --to <ip:port>` instead.
