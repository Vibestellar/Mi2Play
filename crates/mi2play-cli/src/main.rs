use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use clap::{Parser, ValueEnum};
use mdns_sd::{ServiceDaemon, ServiceInfo};
use mi2play_airplay::{PcmAudioFrame, PcmToRaop, RaopClient, RaopEndpoint, discover_raop};
use mi2play_core::DeviceRegistry;
use mi2play_miplay::{MiPlayServer, SessionEvent};
use serde::Deserialize;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::{fs, net::SocketAddr, path::PathBuf};
use thiserror::Error;
use tokio::io::{AsyncReadExt, stdin};
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

#[derive(Clone, Copy, Debug, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "mi2play",
    version,
    about = "Bridge MiPlay virtual devices to AirPlay players"
)]
struct Args {
    #[arg(long, env = "MI2PLAY_CONFIG")]
    config: Option<PathBuf>,
    #[arg(long, default_value = "0.0.0.0:8899")]
    listen: String,
    #[arg(long, default_value = "Mi2Play")]
    name: String,
    #[arg(long, value_delimiter = ',')]
    player_filter: Vec<String>,
    #[arg(long, value_delimiter = ',')]
    player_addresses: Vec<String>,
    #[arg(long, help = "RAOP RTSP address, for example 192.168.1.20:7000")]
    raop_address: Option<SocketAddr>,
    #[arg(long, env = "MI2PLAY_RAOP_PASSWORD")]
    raop_password: Option<String>,
    #[arg(long, default_value_t = 48_000)]
    audio_sample_rate: u32,
    #[arg(long, default_value_t = 2)]
    audio_channels: u8,
    #[arg(long, help = "read interleaved little-endian 16-bit PCM from stdin")]
    pcm_stdin: bool,
    #[arg(long, default_value_t = 0)]
    device_count: usize,
    #[arg(long, default_value_t = true)]
    foreground: bool,
    #[arg(long, default_value_t = true)]
    low_latency: bool,
    #[arg(long, default_value_t = 120)]
    buffer_ms: u32,
    #[arg(long, default_value_t = 1500)]
    max_buffer_ms: u32,
    #[arg(long, default_value_t = true)]
    ipv4: bool,
    #[arg(long, default_value_t = false)]
    ipv6: bool,
    #[arg(long, value_enum, default_value_t = LogLevel::Info)]
    log_level: LogLevel,
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    listen: Option<String>,
    name: Option<String>,
    player_filter: Option<Vec<String>>,
    player_addresses: Option<Vec<String>>,
    raop_address: Option<SocketAddr>,
    raop_password: Option<String>,
    audio_sample_rate: Option<u32>,
    audio_channels: Option<u8>,
    pcm_stdin: Option<bool>,
    device_count: Option<usize>,
    foreground: Option<bool>,
    low_latency: Option<bool>,
    buffer_ms: Option<u32>,
    max_buffer_ms: Option<u32>,
    ipv4: Option<bool>,
    ipv6: Option<bool>,
    log_level: Option<LogLevel>,
}

#[derive(Debug, Error)]
enum ConfigError {
    #[error("failed to read config {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid config {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("buffer_ms must be between 1 and max_buffer_ms")]
    InvalidBuffer,
    #[error("player_filter and player_addresses have no intersection")]
    EmptyPlayerIntersection,
}

fn load_file(path: Option<&PathBuf>) -> Result<FileConfig, ConfigError> {
    let Some(path) = path else {
        return Ok(FileConfig::default());
    };
    let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.clone(),
        source,
    })?;
    toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.clone(),
        source,
    })
}

fn validate(args: &Args, file: &FileConfig) -> Result<(), ConfigError> {
    let buffer = file.buffer_ms.unwrap_or(args.buffer_ms);
    let max = file.max_buffer_ms.unwrap_or(args.max_buffer_ms);
    if buffer == 0 || buffer > max {
        return Err(ConfigError::InvalidBuffer);
    }
    let filters = file.player_filter.as_ref().unwrap_or(&args.player_filter);
    let addresses = file
        .player_addresses
        .as_ref()
        .unwrap_or(&args.player_addresses);
    if !filters.is_empty()
        && !addresses.is_empty()
        && !filters
            .iter()
            .any(|filter| addresses.iter().any(|address| address.contains(filter)))
    {
        return Err(ConfigError::EmptyPlayerIntersection);
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let file = load_file(args.config.as_ref())?;
    validate(&args, &file)?;
    let level = file.log_level.unwrap_or(args.log_level);
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(level.as_str()))
        .init();

    let _registry = DeviceRegistry::default();
    let configured_raop = file.raop_address.or(args.raop_address);
    let raop_password = file.raop_password.or(args.raop_password);
    let sample_rate = file.audio_sample_rate.unwrap_or(args.audio_sample_rate);
    let channels = file.audio_channels.unwrap_or(args.audio_channels);
    let pcm_stdin = file.pcm_stdin.unwrap_or(args.pcm_stdin);
    let listen = file
        .listen
        .as_deref()
        .unwrap_or(&args.listen)
        .parse::<SocketAddr>()?;
    let _mdns = advertise_miplay(
        file.name.as_deref().unwrap_or(&args.name),
        listen,
        file.ipv6.unwrap_or(args.ipv6),
    )?;
    let player_filter = file.player_filter.as_ref().unwrap_or(&args.player_filter);
    let raop_address = if configured_raop.is_some() {
        configured_raop
    } else {
        let discovered =
            tokio::task::spawn_blocking(|| discover_raop(std::time::Duration::from_secs(3)))
                .await??;
        discovered
            .into_iter()
            .find(|endpoint| {
                player_filter.is_empty()
                    || player_filter
                        .iter()
                        .any(|filter| endpoint.name.contains(filter))
            })
            .map(|endpoint| {
                tracing::info!(
                    name = %endpoint.name,
                    address = %endpoint.address,
                    "AirPlay 1 receiver discovered"
                );
                endpoint.address
            })
    };
    tracing::info!(
        listen = %file.listen.as_deref().unwrap_or(&args.listen),
        name = %file.name.as_deref().unwrap_or(&args.name),
        low_latency = file.low_latency.unwrap_or(args.low_latency),
        buffer_ms = file.buffer_ms.unwrap_or(args.buffer_ms),
        max_buffer_ms = file.max_buffer_ms.unwrap_or(args.max_buffer_ms),
        device_count = file.device_count.unwrap_or(args.device_count),
        foreground = file.foreground.unwrap_or(args.foreground),
        ipv4 = file.ipv4.unwrap_or(args.ipv4),
        ipv6 = file.ipv6.unwrap_or(args.ipv6),
        raop_address = ?raop_address,
        sample_rate,
        channels,
        "mi2play protocol core initialized"
    );
    if pcm_stdin {
        let address = raop_address.ok_or("`--pcm-stdin` requires `--raop-address`")?;
        let endpoint = RaopEndpoint {
            address,
            name: "configured-player".into(),
        };
        let mut client = RaopClient::connect_with_password(
            &endpoint,
            sample_rate,
            channels,
            raop_password.as_deref(),
        )
        .await?;
        let mut bridge = PcmToRaop::new(sample_rate, channels)?;
        let bytes_per_frame = channels as usize * 2;
        let mut input = stdin();
        let mut buffer = vec![0; 16 * 1024];
        let mut pending = Vec::new();
        let mut sequence = 0u16;
        let mut timestamp = 0u32;
        loop {
            let read = input.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            pending.extend_from_slice(&buffer[..read]);
            let aligned = pending.len() / bytes_per_frame * bytes_per_frame;
            if aligned == 0 {
                continue;
            }
            let pcm = pending.drain(..aligned).collect::<Vec<_>>();
            client
                .send_pcm(
                    &mut bridge,
                    PcmAudioFrame {
                        sequence,
                        timestamp,
                        sample_rate,
                        channels,
                        pcm,
                    },
                )
                .await?;
            sequence = sequence.wrapping_add(1);
            timestamp = timestamp.wrapping_add((aligned / bytes_per_frame) as u32);
        }
        if !pending.is_empty() {
            return Err("stdin PCM ended with a partial sample".into());
        }
        client.stop().await?;
    } else if let Some(address) = raop_address {
        let server = MiPlayServer::bind(listen).await?;
        tracing::info!(address = %server.local_addr()?, "MiPlay session listener started");
        loop {
            let session = server.accept().await?;
            let endpoint = RaopEndpoint {
                address,
                name: "configured-player".into(),
            };
            let password = raop_password.clone();
            tokio::spawn(async move {
                if let Err(error) =
                    bridge_session(session, endpoint, sample_rate, channels, password).await
                {
                    tracing::warn!(%error, "MiPlay-to-RAOP session failed");
                }
            });
        }
    }
    Ok(())
}

fn advertise_miplay(
    name: &str,
    listen: SocketAddr,
    ipv6: bool,
) -> Result<ServiceDaemon, Box<dyn std::error::Error>> {
    let mut hasher = DefaultHasher::new();
    name.hash(&mut hasher);
    listen.ip().hash(&mut hasher);
    let id = format!("{:016x}", hasher.finish());
    let hostname = format!("mi2play-{id}.local.");
    let instance = format!("{}({})", sanitize_dns_label(name), &id[..10]);
    let apps_data = build_apps_data(listen.port(), &id);
    let properties = [
        ("version", "65545"),
        ("apps", "[5]"),
        ("flags", "Ag=="),
        ("name", name),
        ("idHash", "bWky"),
        ("dev", "4"),
        ("sec", "2"),
        ("appsData", &apps_data),
        ("commonData", "0"),
    ];
    let daemon = ServiceDaemon::new()?;
    if !ipv6 {
        daemon.disable_interface(mdns_sd::IfKind::IPv6)?;
    }
    for (service_type, service_name, port) in [
        ("_lyra-mdns._udp.local.", "_lyra-mdns._udp.local", 5353),
        ("_mi-connect._udp.local.", instance.as_str(), 56666),
    ] {
        let service = ServiceInfo::new(
            service_type,
            service_name,
            &hostname,
            "",
            port,
            &properties[..],
        )?
        .enable_addr_auto();
        daemon.register(service)?;
    }
    tracing::info!(
        name = instance,
        hostname,
        control_port = listen.port(),
        discovery_port = 56666,
        "MiPlay mDNS services advertised"
    );
    Ok(daemon)
}

fn sanitize_dns_label(value: &str) -> String {
    let mut label: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '-'
            }
        })
        .collect();
    if label.is_empty() {
        label.push_str("Mi2Play");
    }
    label.truncate(45);
    label.trim_matches('-').to_owned()
}

fn build_apps_data(control_port: u16, id: &str) -> String {
    let mut payload = vec![0u8; 25];
    payload[0..2].copy_from_slice(&1155u16.to_be_bytes());
    payload[2..4].copy_from_slice(&control_port.to_be_bytes());
    for (index, byte) in id.as_bytes().iter().take(6).enumerate() {
        payload[5 + index] = *byte;
    }
    let identity = format!(r#"{{"mico":{{"device_id":"{}"}}}}"#, id);
    payload.extend_from_slice(identity.as_bytes());
    let mut container = vec![0x81, 0x00, payload.len() as u8];
    container.extend_from_slice(&payload);
    BASE64.encode(container)
}

async fn bridge_session(
    session: mi2play_miplay::MiPlaySession,
    endpoint: RaopEndpoint,
    sample_rate: u32,
    channels: u8,
    password: Option<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let peer = session.peer_addr();
    let (events_tx, mut events_rx) = mpsc::channel(32);
    tokio::spawn(async move {
        if let Err(error) = session.run(events_tx).await {
            tracing::debug!(%error, %peer, "MiPlay connection closed");
        }
    });
    let mut client =
        RaopClient::connect_with_password(&endpoint, sample_rate, channels, password.as_deref())
            .await?;
    let mut bridge = PcmToRaop::new(sample_rate, channels)?;
    while let Some(event) = events_rx.recv().await {
        match event {
            SessionEvent::Audio(frame) => {
                tracing::trace!(
                    sequence = frame.sequence,
                    timestamp = frame.timestamp,
                    pcm_bytes = frame.pcm.len(),
                    "Bridging MiPlay PCM to RAOP"
                );
                client
                    .send_pcm(
                        &mut bridge,
                        PcmAudioFrame {
                            sequence: frame.sequence,
                            timestamp: frame.timestamp,
                            sample_rate: frame.sample_rate,
                            channels: frame.channels,
                            pcm: frame.pcm,
                        },
                    )
                    .await?;
            }
            SessionEvent::Command { command, payload } => match command & 0xff {
                0x04 => client.pause().await?,
                0x06 => client.resume().await?,
                0x0c => {
                    if let Some(&volume) = payload.first() {
                        tracing::debug!(volume, "Applying MiPlay volume to RAOP");
                        client.set_volume(RaopClient::volume_db(volume)).await?;
                    }
                }
                0x08 => break,
                _ => {}
            },
            SessionEvent::Disconnected => break,
        }
    }
    client.stop().await?;
    Ok(())
}
