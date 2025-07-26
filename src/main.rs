use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use clap::{Parser, Subcommand};
use futures_lite::stream::StreamExt;
use iroh::net::{key::SecretKey, Endpoint, NodeAddr};
use std::net::SocketAddr;
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

const ALPN_MINECRAFT: &[u8] = b"iroh-minecraft-proxy";
const MC_SERVER_ADDR: &str = "127.0.0.1:25565";
const LOCAL_LISTEN_ADDR: &str = "127.0.0.1:25565";

#[derive(Parser, Debug)]
#[command(
    name = "iroh-minecraft",
    about = "Proxies Minecraft traffic over Iroh for NAT traversal."
)]
struct Args {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Proxy,
    Client(ClientArgs),
}

#[derive(Parser, Debug)]
struct ClientArgs {
    /// The NodeAddr JSON string of the proxy to connect to.
    proxy_node_addr: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();
    match args.command {
        Commands::Proxy => run_proxy().await,
        Commands::Client(client_args) => run_client(client_args).await,
    }
}

async fn run_proxy() -> Result<()> {
    let secret_key = load_or_generate_secret("proxy_secret.key")?;
    let endpoint = Endpoint::builder()
        .secret_key(secret_key.clone())
        .discovery_n0()
        .bind()
        .await?;

    let direct_addresses: Vec<SocketAddr> = endpoint
        .direct_addresses()
        .next()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|da| da.addr)
        .collect();

    let mut node_addr = NodeAddr::new(endpoint.node_id()).with_direct_addresses(direct_addresses);
    if let Some(relay_url) = endpoint.home_relay() {
        node_addr = node_addr.with_relay_url(relay_url);
    }

    let node_addr_str = serde_json::to_string_pretty(&node_addr)?;
    println!("\n==================================================================");
    println!("Proxy is running. Share this full NodeAddr JSON with clients:");
    println!("\n{}\n", node_addr_str);
    println!("==================================================================");
    println!("Proxy will forward traffic to: {}", MC_SERVER_ADDR);
    println!("Waiting for client connections...");

    while let Some(connecting) = endpoint.accept().await {
        println!("Accepted an incoming connection attempt...");
        tokio::spawn(async move {
            if let Err(e) = handle_proxy_connection(connecting).await {
                eprintln!("[ERROR] Proxy connection failed: {e:?}");
            }
        });
    }

    Ok(())
}

async fn handle_proxy_connection(connecting: iroh::net::endpoint::Incoming) -> Result<()> {
    println!("Establishing connection...");
    let connection = connecting.await.context("Failed to establish connection")?;
    let node_id = connection.stable_id();
    println!("[{}] Connection established.", node_id);

    let (mut send_stream, mut recv_stream) = connection
        .accept_bi()
        .await
        .context("Failed to accept bidirectional stream")?;
    println!("[{}] Bidirectional stream opened.", node_id);

    let server_stream = TcpStream::connect(MC_SERVER_ADDR).await.with_context(|| {
        format!(
            "Failed to connect to Minecraft server at {}",
            MC_SERVER_ADDR
        )
    })?;
    let (mut server_reader, mut server_writer) = tokio::io::split(server_stream);
    println!("[{}] Connected to local Minecraft server.", node_id);

    let iroh_to_server = async {
        tokio::io::copy(&mut recv_stream, &mut server_writer).await?;
        server_writer.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    };

    let server_to_iroh = async {
        tokio::io::copy(&mut server_reader, &mut send_stream).await?;
        send_stream
            .finish()
            .context("Failed to finish Iroh send stream")?;
        Ok::<_, anyhow::Error>(())
    };

    tokio::select! {
        res = iroh_to_server => {
            if let Err(e) = res { eprintln!("[{}] Error proxying Iroh->Server: {e:?}", node_id); }
        },
        res = server_to_iroh => {
            if let Err(e) = res { eprintln!("[{}] Error proxying Server->Iroh: {e:?}", node_id); }
        },
    }

    println!("[{}] Connection finished.", node_id);
    Ok(())
}

async fn run_client(args: ClientArgs) -> Result<()> {
    let proxy_node_addr: NodeAddr = serde_json::from_str(&args.proxy_node_addr)
        .context("Failed to parse proxy NodeAddr JSON string")?;

    let secret_key = load_or_generate_secret("client_secret.key")?;
    let endpoint = Endpoint::builder()
        .secret_key(secret_key)
        .discovery_n0()
        .bind()
        .await?;
    println!(
        "Client endpoint running. Our Node ID is {}",
        endpoint.node_id()
    );

    let listener = tokio::net::TcpListener::bind(LOCAL_LISTEN_ADDR)
        .await
        .with_context(|| format!("Failed to listen on {}", LOCAL_LISTEN_ADDR))?;
    println!("\nClient is ready. Start your Minecraft game and connect to:");
    println!("Server Address: {}", LOCAL_LISTEN_ADDR);
    println!("Waiting for Minecraft client to connect...");

    loop {
        let (tcp_stream, _addr) = listener.accept().await?;
        println!("Accepted connection from local Minecraft client");

        let endpoint_clone = endpoint.clone();
        let proxy_node_addr_clone = proxy_node_addr.clone();

        tokio::spawn(async move {
            if let Err(e) =
                handle_client_connection(tcp_stream, endpoint_clone, proxy_node_addr_clone).await
            {
                eprintln!("[ERROR] Client connection failed: {e:?}");
            }
        });
    }
}

async fn handle_client_connection(
    client_stream: TcpStream,
    endpoint: Endpoint,
    proxy_node_addr: NodeAddr,
) -> Result<()> {
    let proxy_node_id = proxy_node_addr.node_id;
    println!("[{}] Connecting to proxy...", proxy_node_id);

    endpoint
        .add_node_addr(proxy_node_addr)
        .context("Failed to add proxy node address")?;
    let connection = endpoint
        .connect(proxy_node_id, ALPN_MINECRAFT)
        .await
        .context("Failed to connect to proxy node")?;

    println!("[{}] Connected to proxy.", proxy_node_id);

    let (mut send_stream, mut recv_stream) = connection
        .open_bi()
        .await
        .context("Failed to open bidirectional stream")?;
    let (mut client_reader, mut client_writer) = tokio::io::split(client_stream);
    println!("[{}] Bidirectional stream opened.", proxy_node_id);

    let client_to_iroh = async {
        tokio::io::copy(&mut client_reader, &mut send_stream).await?;
        send_stream
            .finish()
            .context("Failed to finish Iroh send stream")?;
        Ok::<_, anyhow::Error>(())
    };

    let iroh_to_client = async {
        tokio::io::copy(&mut recv_stream, &mut client_writer).await?;
        client_writer.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    };

    tokio::select! {
        res = client_to_iroh => {
             if let Err(e) = res { eprintln!("[{}] Error proxying Client->Iroh: {e:?}", proxy_node_id); }
        },
        res = iroh_to_client => {
             if let Err(e) = res { eprintln!("[{}] Error proxying Iroh->Client: {e:?}", proxy_node_id); }
        },
    }

    println!("[{}] Connection finished.", proxy_node_id);
    Ok(())
}

fn load_or_generate_secret(path_str: &str) -> Result<SecretKey> {
    let path = PathBuf::from(path_str);
    if path.exists() {
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read secret key from {}", path.display()))?;
        let bytes = STANDARD
            .decode(contents.trim())
            .context("Failed to decode base64 secret key")?;
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|v: Vec<u8>| anyhow!("Invalid key length: got {}, expected 32", v.len()))?;
        Ok(SecretKey::from_bytes(&arr))
    } else {
        println!("Generating new secret key at {}", path.display());
        let sk = SecretKey::generate();
        let enc = STANDARD.encode(sk.to_bytes());
        std::fs::write(&path, enc)
            .with_context(|| format!("Failed to write secret key to {}", path.display()))?;
        Ok(sk)
    }
}
