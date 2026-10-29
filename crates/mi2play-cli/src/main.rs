use clap::{Parser, ValueEnum};
use mi2play_core::DeviceRegistry;
use serde::Deserialize;
use std::{fs, path::PathBuf};
use thiserror::Error;
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
    #[arg(long, default_value = "0.0.0.0")]
    listen: String,
    #[arg(long, default_value = "Mi2Play")]
    name: String,
    #[arg(long, value_delimiter = ',')]
    player_filter: Vec<String>,
    #[arg(long, value_delimiter = ',')]
    player_addresses: Vec<String>,
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let file = load_file(args.config.as_ref())?;
    validate(&args, &file)?;
    let level = file.log_level.unwrap_or(args.log_level);
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(level.as_str()))
        .init();

    let _registry = DeviceRegistry::default();
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
        "mi2play protocol core initialized; network discovery/output are not enabled yet"
    );
    Ok(())
}
