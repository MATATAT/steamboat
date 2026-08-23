# steamboat v1 — Design

## Purpose

Copy files between machines on the same LAN, motivated by moving emulation
content (ROMs, BIOS files, saves) from macOS/Windows onto a SteamOS machine
whose ext4-casefolding microSD cards other platforms cannot read. The tool is
general-purpose: it transfers any files/directories and knows nothing about
emulation content.

Target platforms: macOS, Windows, Linux (SteamOS). One static binary per
platform, run from the user's home directory, no installation step — it must
survive SteamOS's read-only, wholesale-replaced root filesystem.

## Decisions

- **Hand-off spec is a starting point**, not settled law; its architecture
  choices were re-examined and largely adopted.
- **Run model: receiver session.** No daemon. The receiver is started on
  demand (`steamboat receive`), the sender pushes to it. Nothing runs when no
  transfer is happening.
- **Direction: one-way in v1** (push toward the receiver). The protocol is
  direction-agnostic (manifest + streams), so a reverse path stays cheap if it
  is ever wanted, but no bidirectional work now.
- **Trust model: trusted LAN + receiver confirmation.** Plain TCP, no
  encryption or authentication. The receiver shows who is connecting and what
  they want to send, and the user confirms before anything is written. This
  protects against accidents, not attackers. QUIC/TLS with TOFU pairing is a
  possible v2, not a v1 concern.
- **Scope: general-purpose.** Send any files/directories; the receiver picks
  the destination directory. No destination presets, no emulation awareness.
- **Resumption: file-level skip.** On retry, files already present and intact
  at the destination are skipped via manifest hashes. Chunk-level (mid-file)
  resume is deferred.
- **Interface: CLI on both ends** (`send` / `receive` subcommands, indicatif
  progress bars). A local web app driving the sender is a future idea; the
  lib/bin split keeps that possible.
- **I/O foundation: tokio async.** The protocol layer is written on async I/O
  once, so a v2 QUIC transport (`quinn`, which requires tokio) swaps in
  underneath without rewriting the protocol.
- **Name: steamboat.** A common word (no trademark tangle with Valve's
  SteamPipe or Turbot's Steampipe) with a deliberate nod to Steam for
  discoverability.

## Structure

Single crate `steamboat`, `lib.rs` + `src/bin/main.rs` — the binary is a thin
CLI over the library. Modules:

- **`discovery`** — advertise (receive side) and browse (send side)
  `_steamboat._tcp.local.` via `mdns-sd`, instance-named after the hostname.
  `--to <ip:port>` bypasses discovery entirely.
- **`protocol`** — wire types (hello, manifest, want-list, file header,
  completion) as serde structs, `postcard`-encoded in u32 length-prefixed
  frames. Owns the rule that wire paths are relative, forward-slash separated,
  NFC-normalized.
- **`sender`** / **`receiver`** — the two sides of the transfer state machine
  on tokio TCP.
- **`fsx`** — filesystem concerns: walking source trees, blake3 hashing,
  receiver-side filename sanitization, collision detection, atomic
  write-then-rename.
- **`cli`** (binary side) — clap parsing, indicatif progress, the receiver's
  confirmation prompt.

Dependencies beyond the template baseline: `mdns-sd`, `postcard`, `blake3`,
`unicode-normalization`, `clap`, `indicatif`. All pinned to exact versions per
workspace convention; crate-level `Result` alias over `anyhow::Error`.

## Protocol and data flow

One TCP connection per transfer, sender-initiated, receiver in control of
what gets written:

1. **Receiver**: `steamboat receive <dest-dir>` binds an OS-assigned TCP port
   and advertises it over mDNS. It prints its own name and address so the
   manual fallback is always visible.
2. **Sender**: `steamboat send <paths...> [--to <ip:port>]` walks the
   source paths, hashes every file (blake3), and browses mDNS. One peer found
   → use it; several → numbered pick list; none → suggest `--to`.
3. **Handshake + manifest**: sender connects, sends hello (protocol version,
   hostname), then the manifest (wire paths, sizes, hashes). Version mismatch
   fails cleanly here.
4. **Confirm + want-list**: receiver shows a summary ("`macbook` wants to
   send 412 files, 3.8 GB") and prompts y/n. On yes it scans the destination:
   files whose sanitized path exists with a matching hash are skipped; the
   rest become a want-list of manifest indices sent back. This single
   mechanism is both the confirmation gate and the file-level resume.
5. **Streaming**: sender streams wanted files sequentially — per-file header
   frame, then raw fixed-size data frames (file bytes are never
   postcard-serialized). Receiver writes to `<name>.part`, verifies the hash,
   renames into place. A file that exists with a different hash is
   overwritten — the receiver confirmed the transfer, and re-pushing updated
   files (saves) is a core use.
6. **Completion**: receiver sends a final summary frame (received / skipped /
   failed counts); both ends print it. Progress bars on both sides are driven
   by byte counts against manifest totals.

Failure behavior follows from atomicity: a dropped connection leaves at most
one orphaned `.part` file (deleted on the next run), completed files stay,
and rerunning the same `send` re-sends only what is missing.

## Cross-platform handling

- **Wire paths** are always relative, forward-slash separated, NFC-normalized
  (sender side). Endpoints convert to native paths via `PathBuf` components;
  a wire path never touches the filesystem directly.
- **Sanitization on the receiver, at write time**, on Windows receivers only:
  illegal characters (`: * ? " < > |`), trailing dots/spaces, and reserved
  device names (`CON`, `COM1`, ...) are rewritten with `_` substitution.
  Changed names are printed in the receiver's summary — never silent. No
  persistent mapping table in v1; lossless round-trips only matter for a
  bidirectional future.
- **Case/normalization collisions are detected, not assumed**: before
  writing, the receiver checks whether two manifest entries land on the same
  path on its filesystem (casefolded `Mario.sfc` / `mario.sfc`, NFC/NFD
  twins). Collisions are reported and those files skipped rather than
  silently clobbering each other.
- **Path traversal**: wire paths containing `..` or absolute components are
  rejected at manifest validation. Trusted LAN or not, a malformed manifest
  must not write outside the destination.
- **Permissions**: the receiver writes as whoever ran it; no privilege logic.
  Running as the normal user on SteamOS is the documented path.
- **Firewall/mDNS failure modes** are documentation plus the `--to` fallback:
  the README notes the Windows Firewall first-bind prompt and that VPN
  clients commonly eat multicast.

## Testing

- **Unit tests** (`#[cfg(test)]` at file bottom, `pretty_assertions`): NFC
  normalization, Windows sanitization rules, traversal rejection, collision
  detection, manifest/want-list encoding through postcard.
- **Integration tests**: wire-path round-tripping (native → wire → native),
  and full sender→receiver transfers over `127.0.0.1` in tokio tests —
  direct address, no mDNS — using tempdirs on both ends. Cases: clean
  transfer; retry-after-partial (destination pre-seeded with some correct
  files and one corrupt one, assert only the right files land on the
  want-list); mid-stream disconnect leaving completed files intact.
- **Not automated**: mDNS discovery and the interactive confirm prompt. Both
  get thin seams (the accept decision is a function the CLI wires to a
  prompt, tests wire to a stub), but real multicast behavior is verified
  manually on the actual LAN.

## Out of scope for v1 (possible futures)

- QUIC via `quinn` with trust-on-first-use pairing (encryption/auth)
- Chunk-level (mid-file) resume
- Bidirectional transfer / sync; persistent sanitization mapping table
- Local web app driving the sender
- Destination presets on the receiver
