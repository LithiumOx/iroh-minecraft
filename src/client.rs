use anyhow::{Context, Result};
use base64::Engine;
use clap::Parser;
use iroh_net::{
    discovery::dns::DnsDiscovery,
    key::{PublicKey, SecretKey},
    relay::RelayMode,
    Endpoint, NodeAddr,
};
use std::net::SocketAddr;
use std::str::FromStr;
use tokio::io::{self, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Parser, Debug)]
pub struct ClientArgs {
    #[clap(long, help = "Public key of the proxy to connect to")]
    pub proxy_id: String,
    #[clap(long, help = "Path to secret key file (optional)")]
    pub secret_key: Option<String>,
    #[clap(
        long,
        help = "Local address to listen on for Minecraft client connections"
    )]
    pub listen_addr: SocketAddr,
}

pub async fn run_client(args: &ClientArgs) -> Result<()> {
    // Load or generate secret key
    let secret_key = match &args.secret_key {
        Some(path) => {
            let contents = std::fs::read_to_string(path)?;
            let key_bytes_vec =
                base64::engine::general_purpose::STANDARD.decode(contents.trim())?;
            let key_bytes: [u8; 32] = key_bytes_vec
                .try_into()
                .map_err(|_| anyhow::anyhow!("Invalid key length"))?;
            SecretKey::from_bytes(&key_bytes)
        }
        None => SecretKey::generate(),
    };

    // Create endpoint
    let endpoint = Endpoint::builder()
        .secret_key(secret_key)
        .relay_mode(RelayMode::Default)
        .discovery(Box::new(DnsDiscovery::n0_dns()))
        .bind()
        .await?;

    let proxy_id =
        PublicKey::from_str(&args.proxy_id).context("Failed to parse proxy_id as PublicKey")?;

    let addr = NodeAddr::new(proxy_id);

    println!("Client started!");
    println!("Connecting to proxy: {}", proxy_id);
    println!("Listening for Minecraft client on: {}", args.listen_addr);

    // Listen for local Minecraft client connections
    let listener = TcpListener::bind(args.listen_addr)
        .await
        .context("Failed to bind to listen address")?;

    loop {
        let (client_stream, client_addr) = listener.accept().await?;
        println!("New Minecraft client connection from: {}", client_addr);

        let addr = addr.clone();
        let endpoint = endpoint.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_client_connection(client_stream, addr, endpoint).await {
                eprintln!("Client connection error: {:?}", e);
            }
        });
    }
}

async fn handle_client_connection(
    client_stream: tokio::net::TcpStream,
    proxy_addr: NodeAddr,
    endpoint: Endpoint,
) -> Result<()> {
    // Connect to the proxy through Iroh
    println!("Connecting to proxy...");
    let conn = endpoint
        .connect(proxy_addr, b"minecraft")
        .await
        .context("Failed to connect to proxy")?;

    println!("Connected to proxy successfully!");

    let (mut send, mut recv) = conn.open_bi().await?;
    let (mut client_reader, mut client_writer) = io::split(client_stream);

    // Forward data from local client to remote proxy
    let local_to_remote = async {
        let mut buf = [0; 4096];
        loop {
            match client_reader.read(&mut buf).await {
                Ok(0) => {
                    println!("Client disconnected");
                    break;
                }
                Ok(n) => {
                    if let Err(e) = send.write_all(&buf[..n]).await {
                        eprintln!("Error writing to proxy: {:?}", e);
                        break;
                    }
                }
                Err(e) => {
                    eprintln!("Error reading from client: {:?}", e);
                    break;
                }
            }
        }
        if let Err(e) = send.finish() {
            eprintln!("Error finishing send stream: {:?}", e);
        }
        Ok::<(), anyhow::Error>(())
    };

    // Forward data from remote proxy to local client
    let remote_to_local = async {
        let mut buf = [0; 4096];
        loop {
            match recv.read(&mut buf).await {
                Ok(Some(n)) => {
                    if n == 0 {
                        break;
                    }
                    if let Err(e) = client_writer.write_all(&buf[..n]).await {
                        eprintln!("Error writing to client: {:?}", e);
                        break;
                    }
                }
                Ok(None) => {
                    println!("Proxy connection closed");
                    break;
                }
                Err(e) => {
                    eprintln!("Error reading from proxy: {:?}", e);
                    break;
                }
            }
        }
        if let Err(e) = client_writer.shutdown().await {
            eprintln!("Error shutting down client writer: {:?}", e);
        }
        Ok::<(), anyhow::Error>(())
    };

    // Run both tasks concurrently
    tokio::select! {
        result = local_to_remote => {
            if let Err(e) = result {
                eprintln!("Local to remote task failed: {:?}", e);
            }
        },
        result = remote_to_local => {
            if let Err(e) = result {
                eprintln!("Remote to local task failed: {:?}", e);
            }
        }
    }

    println!("Connection handling completed");
    Ok(())
}
