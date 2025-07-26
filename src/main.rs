use anyhow::Result;
use base64::Engine;
use clap::{Parser, Subcommand};
use iroh_net::{endpoint::Connection, key::SecretKey, Endpoint};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{self, AsyncReadExt, AsyncWriteExt};

#[derive(Parser, Debug)]
#[clap(name = "iroh-minecraft")]
struct Args {
    #[clap(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Client(iroh_minecraft::client::ClientArgs),
    Proxy(ProxyArgs),
}

#[derive(Parser, Debug)]
struct ProxyArgs {
    #[clap(long)]
    pub listen_addr: SocketAddr,
    #[clap(long)]
    pub server_addr: SocketAddr,
    #[clap(long)]
    pub secret_key: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    match args.command {
        Commands::Client(client_args) => iroh_minecraft::client::run_client(&client_args).await,
        Commands::Proxy(proxy_args) => run_proxy(&proxy_args).await,
    }
}

async fn run_proxy(args: &ProxyArgs) -> Result<()> {
    // Load or generate secret key
    let key_file = args
        .secret_key
        .clone()
        .unwrap_or_else(|| PathBuf::from("secret_key.txt"));

    let secret_key = if key_file.exists() {
        let contents = std::fs::read_to_string(&key_file)?;
        let key_bytes_vec = base64::engine::general_purpose::STANDARD.decode(contents.trim())?;
        let key_bytes: [u8; 32] = key_bytes_vec
            .try_into()
            .map_err(|_| anyhow::anyhow!("Invalid key length"))?;
        SecretKey::from_bytes(&key_bytes)
    } else {
        let secret_key = SecretKey::generate();
        let encoded = base64::engine::general_purpose::STANDARD.encode(secret_key.to_bytes());
        std::fs::write(&key_file, encoded)?;
        secret_key
    };

    let endpoint = Endpoint::builder()
        .secret_key(secret_key.clone())
        .bind()
        .await?;
    let public_key = secret_key.public();

    println!("Proxy started!");
    println!("Public Key: {}", public_key);
    println!("Listening on: {}", args.listen_addr);
    println!("Proxying to: {}", args.server_addr);

    let listener = tokio::net::TcpListener::bind(args.listen_addr).await?;
    let endpoint = Arc::new(endpoint);

    // Handle incoming Iroh connections
    let endpoint_clone = endpoint.clone();
    let server_addr = args.server_addr;
    tokio::spawn(async move {
        while let Some(incoming) = endpoint_clone.accept().await {
            let conn = match incoming.await {
                Ok(conn) => conn,
                Err(e) => {
                    eprintln!("Failed to accept incoming connection: {:?}", e);
                    continue;
                }
            };

            tokio::spawn(async move {
                if let Err(e) = handle_iroh_connection(conn, server_addr).await {
                    eprintln!("Iroh connection error: {:?}", e);
                }
            });
        }
    });

    // Handle incoming TCP connections (from local Minecraft clients)
    loop {
        let (client_stream, addr) = listener.accept().await?;
        println!("New client connection from: {}", addr);

        let server_addr = args.server_addr;
        tokio::spawn(async move {
            if let Err(e) = handle_tcp_connection(client_stream, server_addr).await {
                eprintln!("TCP connection error: {:?}", e);
            }
        });
    }
}

async fn handle_tcp_connection(
    client_stream: tokio::net::TcpStream,
    server_addr: SocketAddr,
) -> Result<()> {
    let server_stream = tokio::net::TcpStream::connect(server_addr).await?;

    let (mut client_reader, mut client_writer) = io::split(client_stream);
    let (mut server_reader, mut server_writer) = io::split(server_stream);

    // Bidirectional proxy between client and server
    let client_to_server = async {
        let mut buf = [0; 4096];
        loop {
            match client_reader.read(&mut buf).await {
                Ok(0) => break, // EOF
                Ok(n) => {
                    if let Err(e) = server_writer.write_all(&buf[..n]).await {
                        eprintln!("Error writing to server: {:?}", e);
                        break;
                    }
                }
                Err(e) => {
                    eprintln!("Error reading from client: {:?}", e);
                    break;
                }
            }
        }
        let _ = server_writer.shutdown().await;
        Ok::<(), anyhow::Error>(())
    };

    let server_to_client = async {
        let mut buf = [0; 4096];
        loop {
            match server_reader.read(&mut buf).await {
                Ok(0) => break, // EOF
                Ok(n) => {
                    if let Err(e) = client_writer.write_all(&buf[..n]).await {
                        eprintln!("Error writing to client: {:?}", e);
                        break;
                    }
                }
                Err(e) => {
                    eprintln!("Error reading from server: {:?}", e);
                    break;
                }
            }
        }
        let _ = client_writer.shutdown().await;
        Ok::<(), anyhow::Error>(())
    };

    tokio::select! {
        _ = client_to_server => {},
        _ = server_to_client => {},
    }

    Ok(())
}

async fn handle_iroh_connection(conn: Connection, server_addr: SocketAddr) -> Result<()> {
    let (mut send, mut recv) = conn.accept_bi().await?;
    let server_stream = tokio::net::TcpStream::connect(server_addr).await?;
    let (mut server_reader, mut server_writer) = io::split(server_stream);

    // Forward data from Iroh to server
    let iroh_to_server = async {
        let mut buf = [0; 4096];
        loop {
            match recv.read(&mut buf).await {
                Ok(Some(n)) => {
                    if n == 0 {
                        break;
                    }
                    if let Err(e) = server_writer.write_all(&buf[..n]).await {
                        eprintln!("Error writing to server: {:?}", e);
                        break;
                    }
                }
                Ok(None) => break, // EOF
                Err(e) => {
                    eprintln!("Error reading from Iroh: {:?}", e);
                    break;
                }
            }
        }
        let _ = server_writer.shutdown().await;
        Ok::<(), anyhow::Error>(())
    };

    // Forward data from server to Iroh
    let server_to_iroh = async {
        let mut buf = [0; 4096];
        loop {
            match server_reader.read(&mut buf).await {
                Ok(0) => break, // EOF
                Ok(n) => {
                    if let Err(e) = send.write_all(&buf[..n]).await {
                        eprintln!("Error writing to Iroh: {:?}", e);
                        break;
                    }
                }
                Err(e) => {
                    eprintln!("Error reading from server: {:?}", e);
                    break;
                }
            }
        }
        if let Err(e) = send.finish() {
            eprintln!("Error finishing Iroh send: {:?}", e);
        }
        Ok::<(), anyhow::Error>(())
    };

    tokio::select! {
        _ = iroh_to_server => {},
        _ = server_to_iroh => {},
    }

    Ok(())
}
