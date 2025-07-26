use anyhow::anyhow;
use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use clap::{Parser, Subcommand};
use iroh::net::{key::SecretKey, Endpoint, NodeAddr};
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

const ALPN: &[u8] = b"iroh-minecraft-proxy";
const MC_ADDR: &str = "127.0.0.1:25565";
const LOCAL_ADDR: &str = "127.0.0.1:25565";

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Proxy,
    Client(ClientArgs),
}

#[derive(Parser)]
struct ClientArgs {
    node_id: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    match Args::parse().command {
        Commands::Proxy => run_proxy().await,
        Commands::Client(c) => run_client(c).await,
    }
}

async fn run_proxy() -> Result<()> {
    let secret = load_or_generate_secret("proxy_secret.key")?;
    let ep = Endpoint::builder()
        .secret_key(secret)
        .discovery_n0()
        .bind()
        .await?;
    let node_id = ep.node_id();
    let mut addr = NodeAddr::new(node_id);
    if let Some(relay) = ep.home_relay() {
        addr = addr.with_relay_url(relay);
    }
    println!("Proxy NodeAddr:\n\n{:?}", addr);
    println!("Forwarding to {}", MC_ADDR);

    while let Some(inc) = ep.accept().await {
        tokio::spawn(async move {
            if let Err(e) = handle_proxy(inc).await {
                eprintln!("[ERROR] {e:?}");
            }
        });
    }
    Ok(())
}

async fn handle_proxy(connecting: iroh::net::endpoint::Incoming) -> Result<()> {
    let conn = connecting.await.context("accept")?;
    let peer = conn.remote_address();
    let (mut send_s, mut recv_s) = conn.accept_bi().await.context("accept_bi")?;
    let mc = TcpStream::connect(MC_ADDR).await.context("connect MC")?;
    let (mut mc_r, mut mc_w) = tokio::io::split(mc);

    let t1 = async {
        tokio::io::copy(&mut recv_s, &mut mc_w).await?;
        mc_w.shutdown().await?;
        Ok::<(), anyhow::Error>(())
    };
    let t2 = async {
        tokio::io::copy(&mut mc_r, &mut send_s).await?;
        send_s.finish()?; // ✅ removed .await
        Ok::<(), anyhow::Error>(())
    };

    tokio::select! {
        res = t1 => if let Err(e) = res { eprintln!("[{}] → MC error: {e:?}", peer); },
        res = t2 => if let Err(e) = res { eprintln!("[{}] MC → {} error: {e:?}", peer, peer); },
    }

    Ok(())
}

async fn run_client(args: ClientArgs) -> Result<()> {
    let secret = load_or_generate_secret("client_secret.key")?;
    let ep = Endpoint::builder()
        .secret_key(secret)
        .discovery_n0()
        .bind()
        .await?;
    println!("Client Node ID: {}", ep.node_id());
    let proxy = NodeAddr::new(args.node_id.parse()?);
    let listener = tokio::net::TcpListener::bind(LOCAL_ADDR).await?;
    println!("Connect Minecraft to {}", LOCAL_ADDR);

    loop {
        let (client, _addr) = listener.accept().await?;
        let ep2 = ep.clone();
        let proxy2 = proxy.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(client, ep2, proxy2).await {
                eprintln!("[ERROR] {}", e);
            }
        });
    }
}

async fn handle_client(client: TcpStream, ep: Endpoint, proxy: NodeAddr) -> Result<()> {
    ep.add_node_addr(proxy.clone())?;
    let conn = ep.connect(proxy.node_id, ALPN).await?;
    let (mut send_s, mut recv_s) = conn.open_bi().await?;
    let (mut cr, mut cw) = tokio::io::split(client);

    let t1 = async {
        tokio::io::copy(&mut cr, &mut send_s).await?;
        send_s.finish()?;
        Ok::<(), anyhow::Error>(())
    };
    let t2 = async {
        tokio::io::copy(&mut recv_s, &mut cw).await?;
        cw.shutdown().await?;
        Ok::<(), anyhow::Error>(())
    };

    tokio::select! { _ = t1 => {}, _ = t2 => {} }

    Ok(())
}

fn load_or_generate_secret(path: &str) -> Result<SecretKey> {
    let path = PathBuf::from(path);
    if path.exists() {
        let s = std::fs::read_to_string(&path).context("Failed to read secret key file")?;
        let bytes = STANDARD
            .decode(s.trim())
            .context("Failed to decode base64 secret key")?;
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|v: Vec<u8>| anyhow!("Invalid key length: got {}", v.len()))?;
        Ok(SecretKey::from_bytes(&arr))
    } else {
        let sk = SecretKey::generate();
        let enc = STANDARD.encode(sk.to_bytes()).to_owned();
        std::fs::write(&path, enc).context("Failed to write secret key")?;
        Ok(sk)
    }
}
