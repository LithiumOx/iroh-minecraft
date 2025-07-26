use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use clap::{Parser, Subcommand};
use iroh::net::{key::SecretKey, Endpoint, NodeId};
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

// An ALPN protocol identifier is required for Iroh connections.
const ALPN_MINECRAFT: &[u8] = b"iroh-minecraft-proxy";
// The default address for a local Minecraft server.
const MC_SERVER_ADDR: &str = "127.0.0.1:25565";
// The default address for the client to listen on.
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
    /// Runs the proxy-side, which connects to a local Minecraft server.
    Proxy,
    /// Runs the client-side, which listens for a local Minecraft client.
    Client(ClientArgs),
}

#[derive(Parser, Debug)]
struct ClientArgs {
    /// The PublicKey (or NodeId) of the proxy to connect to.
    proxy_node_id: NodeId,
}

#[tokio::main]
async fn main() -> Result<()> {
    // This requires `tracing-subscriber` in your Cargo.toml.
    // Run `cargo add tracing-subscriber --features=fmt` if you haven't already.
    tracing_subscriber::fmt::init();

    let args = Args::parse();
    match args.command {
        Commands::Proxy => run_proxy().await,
        Commands::Client(client_args) => run_client(client_args).await,
    }
}

//================================================================================
// Proxy Logic (Runs on the same machine as the Minecraft Server)
//================================================================================

async fn run_proxy() -> Result<()> {
    let secret_key = load_or_generate_secret("proxy_secret.key")?;
    let endpoint = Endpoint::builder()
        .secret_key(secret_key.clone())
        .bind()
        .await?;

    println!("\n==================================================================");
    println!("Proxy Node ID: {}", endpoint.node_id());
    println!("==================================================================\n");
    println!(
        "Proxy is running. It will forward traffic to: {}",
        MC_SERVER_ADDR
    );
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

    // Correctly get the remote NodeId for logging
    let node_id = connection.remote_address();

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

//================================================================================
// Client Logic (Runs on the same machine as the Minecraft Game Client)
//================================================================================

async fn run_client(args: ClientArgs) -> Result<()> {
    let secret_key = load_or_generate_secret("client_secret.key")?;
    let endpoint = Endpoint::builder()
        .secret_key(secret_key)
        .discovery_n0()
        .bind()
        .await?;
    println!(
        "Iroh endpoint running. Our Node ID is {}",
        endpoint.node_id()
    );

    let listener = tokio::net::TcpListener::bind(LOCAL_LISTEN_ADDR)
        .await
        .with_context(|| format!("Failed to listen on {}", LOCAL_LISTEN_ADDR))?;
    println!("\nClient is ready. Start your Minecraft game and connect to:");
    println!("Server Address: {}", LOCAL_LISTEN_ADDR);
    println!("\nWaiting for Minecraft client to connect...");

    loop {
        let (tcp_stream, addr) = listener.accept().await?;
        println!("Accepted connection from local Minecraft client: {}", addr);

        let endpoint_clone = endpoint.clone();
        let proxy_node_id = args.proxy_node_id;

        tokio::spawn(async move {
            if let Err(e) =
                handle_client_connection(tcp_stream, endpoint_clone, proxy_node_id).await
            {
                eprintln!("[ERROR] Client connection failed: {e:?}");
            }
        });
    }
}

async fn handle_client_connection(
    client_stream: TcpStream,
    endpoint: Endpoint,
    proxy_node_id: NodeId,
) -> Result<()> {
    println!("[{}] Connecting to proxy...", proxy_node_id);
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

//================================================================================
// Shared Helper Functions
//================================================================================

/// Loads a `SecretKey` from a file or generates a new one and saves it.
fn load_or_generate_secret(path_str: &str) -> Result<SecretKey> {
    let path = PathBuf::from(path_str);
    if path.exists() {
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read secret key from {}", path.display()))?;
        let key_bytes = STANDARD
            .decode(contents.trim())
            .context("Failed to decode base64 secret key")?;
        let key_array: [u8; 32] = key_bytes.try_into().map_err(|v: Vec<u8>| {
            anyhow!("Invalid key length: expected 32 bytes, got {}", v.len())
        })?;
        Ok(SecretKey::from_bytes(&key_array))
    } else {
        println!("Generating new secret key at {}", path.display());
        let secret_key = SecretKey::generate();
        let encoded = STANDARD.encode(secret_key.to_bytes());
        std::fs::write(&path, encoded)
            .with_context(|| format!("Failed to write secret key to {}", path.display()))?;
        Ok(secret_key)
    }
}
