use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use indicatif::{HumanBytes, ProgressBar, ProgressStyle};
use std::io::Write;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::time::Duration;
use steamboat::receiver::{ReceiveOutcome, TransferOffer};
use steamboat::{Progress, SteamboatResult, discovery, fsx, receiver, sender};
use tokio::net::{TcpListener, TcpStream};
use tokio::task;

#[derive(Parser)]
#[command(name = "steamboat", version, about = "Copy files between machines over the LAN")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Send files or directories to a receiver on the LAN
    Send {
        /// Files and directories to send
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Skip mDNS discovery and connect to this address directly
        #[arg(long)]
        to: Option<SocketAddr>,
    },
    /// Receive files into a destination directory
    Receive {
        /// Directory incoming files are written into
        dest: PathBuf,
    },
}

struct BarProgress {
    bar: ProgressBar,
}

impl BarProgress {
    fn new() -> BarProgress {
        BarProgress {
            bar: ProgressBar::hidden(),
        }
    }
}

impl Progress for BarProgress {
    fn transfer_start(&self, _file_count: u64, total_bytes: u64) {
        self.bar
            .set_length(total_bytes);
        self.bar.set_style(
            ProgressStyle::with_template("{bar:40} {bytes}/{total_bytes} ({bytes_per_sec}, {eta}) {wide_msg}")
                .expect("static template"),
        );
        self.bar
            .set_draw_target(indicatif::ProgressDrawTarget::stderr());
    }

    fn file_start(&self, wire_path: &str) {
        self.bar
            .set_message(wire_path.to_string());
    }

    fn chunk(&self, bytes: u64) {
        self.bar.inc(bytes);
    }
}

fn hostname() -> String {
    gethostname::gethostname()
        .to_string_lossy()
        .into_owned()
}

/// Best-effort local address for display; connecting a UDP socket sends no
/// packets, it just selects a route.
fn local_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket
        .connect("8.8.8.8:80")
        .ok()?;

    socket
        .local_addr()
        .map(|a| a.ip())
        .ok()
}

fn confirm(offer: &TransferOffer) -> bool {
    print!(
        "\n\"{}\" wants to send {} files, {} — accept? [y/N] ",
        offer.peer,
        offer.file_count,
        HumanBytes(offer.total_bytes)
    );
    std::io::stdout().flush().ok();
    let mut line = String::new();

    std::io::stdin()
        .read_line(&mut line)
        .is_ok()
        && matches!(
            line.trim()
                .to_lowercase()
                .as_str(),
            "y" | "yes"
        )
}

fn pick_peer() -> SteamboatResult<SocketAddr> {
    println!("searching for receivers...");
    let peers = discovery::browse(Duration::from_secs(3))?;
    match peers.as_slice() {
        [] => bail!(
            "no receivers found — is `steamboat receive` running? \
             Use --to <ip:port> if mDNS is blocked (VPNs often block it)"
        ),
        [only] => {
            println!("found \"{}\" at {}", only.name, only.addr);

            Ok(only.addr)
        }
        several => {
            for (i, peer) in several.iter().enumerate() {
                println!("  {}: \"{}\" at {}", i + 1, peer.name, peer.addr);
            }
            print!("send to which receiver? [1-{}] ", several.len());
            std::io::stdout().flush().ok();
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            let choice: usize = line
                .trim()
                .parse()
                .context("not a number")?;

            several
                .get(choice.wrapping_sub(1))
                .map(|p| p.addr)
                .context("choice out of range")
        }
    }
}

async fn send_cmd(paths: Vec<PathBuf>, to: Option<SocketAddr>) -> SteamboatResult<()> {
    println!("hashing sources...");
    let files = task::spawn_blocking(move || fsx::walk_sources(&paths)).await??;
    let total: u64 = files
        .iter()
        .map(|f| f.size)
        .sum();
    println!("{} files, {}", files.len(), HumanBytes(total));

    let addr = match to {
        Some(addr) => addr,
        None => task::spawn_blocking(pick_peer).await??,
    };
    let stream = TcpStream::connect(addr)
        .await
        .with_context(|| format!("connecting to {addr}"))?;
    let summary = sender::run_sender(stream, &hostname(), &files, &BarProgress::new()).await?;
    println!(
        "done: {} received, {} already present, {} failed",
        summary.received, summary.skipped, summary.failed
    );

    Ok(())
}

async fn receive_cmd(dest: PathBuf) -> SteamboatResult<()> {
    std::fs::create_dir_all(&dest)?;
    let dest = dest.canonicalize()?;
    let removed = fsx::clean_orphan_parts(&dest)?;
    if removed > 0 {
        println!("cleaned {removed} orphaned .part file(s)");
    }

    let listener = TcpListener::bind(("0.0.0.0", 0)).await?;
    let port = listener.local_addr()?.port();
    let name = hostname();
    let _advertiser = discovery::Advertiser::start(&name, port)?;
    let shown_ip = local_ip().map_or_else(|| "<local-ip>".to_string(), |ip| ip.to_string());
    println!("receiving into {} as \"{name}\"", dest.display());
    println!("direct address (for --to): {shown_ip}:{port}");
    println!("press Ctrl-C to stop\n");

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            accepted = listener.accept() => {
                let (stream, peer_addr) = accepted?;
                println!("connection from {peer_addr}");
                let outcome =
                    receiver::run_receiver(stream, &dest, &name, confirm, &BarProgress::new())
                        .await;
                match outcome {
                    Ok(ReceiveOutcome::Completed(report)) => {
                        for (wire, native) in &report.renamed {
                            println!("renamed for this filesystem: {wire} -> {native}");
                        }
                        for wire in &report.collided {
                            println!("skipped: {wire} collides with another incoming file here");
                        }
                        println!(
                            "done: {} received, {} already present, {} failed\n",
                            report.summary.received, report.summary.skipped, report.summary.failed
                        );
                    }
                    Ok(ReceiveOutcome::Declined) => println!("declined\n"),
                    Err(e) => eprintln!("transfer failed: {e:#}\n"),
                }
            }
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() -> SteamboatResult<()> {
    match Cli::parse().command {
        Command::Send { paths, to } => send_cmd(paths, to).await,
        Command::Receive { dest } => receive_cmd(dest).await,
    }
}
