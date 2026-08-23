# steamboat

Copy files between machines on the same LAN. I originally built this for pushing emulation
content (ROMs, BIOS files, saves) from a Mac or Windows desktop onto a Steam
Machine running SteamOS, where migrating files by normal means was annoying.
There are tools that already solve this problem but I wanted to make something
a little easier. This can also just be used as a geneal p2p file transfer tool.

```
steamboat receive <dest-dir>   # on the machine receiving files
steamboat send <paths...>      # on the machine sending them
```

Peers find each other over mDNS; `--to <ip:port>` works when multicast
doesn't. The receiver confirms every incoming transfer before anything is
written, and retrying an interrupted transfer only re-sends what's missing.

## Troubleshooting

- **Windows Firewall** prompts the first time `steamboat receive` binds a
  port — allow it on private networks or the sender can't connect.
- **Discovery finds nothing?** mDNS needs UDP 5353 multicast; VPN clients and
  some routers silently block it. The receiver prints a direct address — use
  `steamboat send --to <ip:port>` instead.
