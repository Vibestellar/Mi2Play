use aes::Aes128;
use aes::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit, block_padding::NoPadding};
use cbc::{Decryptor, Encryptor};
use hmac::{Hmac, Mac};
use md5::{Digest, Md5};
use rand::RngCore;
use serde_json::Value;
use sha2::Sha256;
use std::net::SocketAddr;
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
};

pub const MAGIC: u8 = 0x24;
pub const FRAME_HEADER_SIZE: usize = 9;
pub const MAX_PAYLOAD: usize = 16 * 1024 * 1024;
pub const CONTROL_PORT: u16 = 8899;

const GET_VERSION: u8 = 0x36;
const GET_VERSION_ACK: u8 = 0x37;
const DEVICE_ID: u8 = 0x28;
const AUTH_ACK: u8 = 0x29;
const SAFETY_INFO: u8 = 0x00;
const SAFETY_INFO_ACK: u8 = 0x01;
const SAFETY_AUTH: u8 = 0x02;
const SAFETY_AUTH_ACK: u8 = 0x03;
const GET_DEVICE_INFO: u8 = 0x1e;
const GET_DEVICE_INFO_ACK: u8 = 0x1f;
const GET_MIRROR_MODE: u8 = 0x34;
const GET_MIRROR_MODE_ACK: u8 = 0x35;
const SET_MIRROR_KEY: u8 = 0x6c;
const SET_MIRROR_KEY_ACK: u8 = 0x6d;
const OPEN: u8 = 0x00;
const OPEN_ACK: u8 = 0x01;
const PAUSE: u8 = 0x04;
const PAUSE_ACK: u8 = 0x05;
const RESUME: u8 = 0x06;
const RESUME_ACK: u8 = 0x07;
const SET_VOLUME: u8 = 0x0c;
const SET_VOLUME_ACK: u8 = 0x0d;
const GET_VOLUME: u8 = 0x0e;
const GET_VOLUME_ACK: u8 = 0x0f;
const SET_MEDIA_INFO: u8 = 0x12;
const SET_MEDIA_INFO_ACK: u8 = 0x13;
const GET_MEDIA_INFO: u8 = 0x14;
const HEARTBEAT: u8 = 0x1a;
const HEARTBEAT_ACK: u8 = 0x1b;
const GET_STATE: u8 = 0x1c;
const GET_STATE_ACK: u8 = 0x1d;
const NOTIFY: u8 = 0x22;
const SET_DEVICE_INFO: u8 = 0x58;
const SET_DEVICE_INFO_ACK: u8 = 0x59;
const SET_PLAY_SOURCE: u8 = 0x40;
const SET_PLAY_SOURCE_ACK: u8 = 0x41;
const SET_POSITION: u8 = 0x56;
const SET_POSITION_ACK: u8 = 0x57;
const SET_MEDIA_STATE: u8 = 0x5e;
const SET_MEDIA_STATE_ACK: u8 = 0x5f;
const CONTROL_VERSION: &str = "2.1.5071614";

type AesCbc = Encryptor<Aes128>;
type AesCbcDecrypt = Decryptor<Aes128>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceIdentity {
    pub id: String,
    pub name: String,
    pub model: String,
}

impl Default for DeviceIdentity {
    fn default() -> Self {
        Self {
            id: "mi2play".into(),
            name: "Mi2Play".into(),
            model: "Mi2Play".into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PcmAudioFrame {
    pub sequence: u16,
    pub timestamp: u32,
    pub sample_rate: u32,
    pub channels: u8,
    pub pcm: Vec<u8>,
}

#[derive(Debug)]
pub enum SessionEvent {
    Audio(PcmAudioFrame),
    Command { command: u16, payload: Vec<u8> },
    Disconnected,
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("invalid MiPlay frame magic")]
    InvalidMagic,
    #[error("MiPlay payload exceeds limit: {0}")]
    PayloadTooLarge(usize),
    #[error("truncated MiPlay frame")]
    Truncated,
    #[error("invalid MiPlay encrypted body")]
    InvalidEncryptedBody,
    #[error("invalid MiPlay JSON payload")]
    InvalidJson,
    #[error("invalid MiPlay URL: {0}")]
    InvalidUrl(String),
    #[error("invalid RTSP message")]
    InvalidRtsp,
    #[error("invalid RTP/MP2T packet")]
    InvalidRtp,
    #[error("AAC decoder error: {0}")]
    Aac(String),
    #[error("unsupported audio format: {0}")]
    UnsupportedAudio(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Debug)]
struct Frame {
    outer: u8,
    command: u8,
    sequence: u16,
    body: Vec<u8>,
    encrypted: bool,
}

#[derive(Clone)]
struct StreamKeys {
    stream_key: [u8; 16],
    stream_iv: [u8; 16],
}

struct Cipher {
    key: [u8; 16],
    iv: [u8; 16],
}

impl Cipher {
    fn new(key: [u8; 16]) -> Self {
        Self { key, iv: key }
    }

    fn encrypt(&mut self, plain: &[u8]) -> Result<Vec<u8>, ProtocolError> {
        let padding = 16 - (plain.len() % 16);
        let mut data = plain.to_vec();
        data.resize(data.len() + padding, 0);
        let next_iv = data[data.len() - 16..].try_into().unwrap_or([0; 16]);
        let encryptor = AesCbc::new_from_slices(&self.key, &self.iv)
            .map_err(|_| ProtocolError::InvalidEncryptedBody)?;
        let data_len = data.len();
        encryptor
            .encrypt_padded_mut::<NoPadding>(&mut data, data_len)
            .map_err(|_| ProtocolError::InvalidEncryptedBody)?;
        self.iv = next_iv;
        let mut result = vec![0, 7, 1, 0xe0, padding as u8];
        result.extend_from_slice(&integrity(&data).to_be_bytes());
        result.extend_from_slice(&data);
        Ok(result)
    }

    fn decrypt(&mut self, body: &[u8]) -> Result<Vec<u8>, ProtocolError> {
        if body.len() < 25 || (body.len() - 9) % 16 != 0 {
            return Err(ProtocolError::InvalidEncryptedBody);
        }
        let padding = body[4] as usize;
        let mut data = body[9..].to_vec();
        if u32::from_be_bytes(body[5..9].try_into().unwrap()) != integrity(&data) {
            return Err(ProtocolError::InvalidEncryptedBody);
        }
        let next_iv = data[data.len() - 16..].try_into().unwrap_or([0; 16]);
        let decryptor = AesCbcDecrypt::new_from_slices(&self.key, &self.iv)
            .map_err(|_| ProtocolError::InvalidEncryptedBody)?;
        decryptor
            .decrypt_padded_mut::<NoPadding>(&mut data)
            .map_err(|_| ProtocolError::InvalidEncryptedBody)?;
        self.iv = next_iv;
        if padding == 0 || padding > 16 || padding > data.len() {
            return Err(ProtocolError::InvalidEncryptedBody);
        }
        data.truncate(data.len() - padding);
        Ok(data)
    }
}

fn integrity(bytes: &[u8]) -> u32 {
    let mut value = u32::MAX;
    for byte in bytes {
        let mut table = u32::from((value as u8) ^ byte) << 24;
        for _ in 0..8 {
            table = if table & 0x8000_0000 != 0 {
                (table << 1) ^ 0x04c1_1db7
            } else {
                table << 1
            };
        }
        value = table.swap_bytes() ^ (value >> 8);
    }
    value
}

fn auth_key(local: SocketAddr, remote: SocketAddr) -> [u8; 16] {
    let value = format!(
        "{}{}{}{}",
        local.ip(),
        local.port(),
        remote.ip(),
        remote.port()
    );
    let mapped: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_digit() {
                char::from_u32(c as u32 + 0x31).unwrap()
            } else {
                c
            }
        })
        .collect();
    let digest = Md5::digest(mapped.as_bytes());
    digest[..16].try_into().unwrap()
}

fn hmac_hex(key: &[u8], value: &str) -> String {
    let mut hmac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts every key");
    hmac.update(value.as_bytes());
    hex::encode(hmac.finalize().into_bytes())
}

fn wrap(key: &str, data: &[u8]) -> Result<Vec<u8>, ProtocolError> {
    if key.len() > 255 || data.len() > 255 {
        return Err(ProtocolError::PayloadTooLarge(data.len()));
    }
    let mut result = vec![key.len() as u8];
    result.extend_from_slice(key.as_bytes());
    result.extend_from_slice(&30u32.to_le_bytes());
    result.push(data.len() as u8);
    result.extend_from_slice(data);
    Ok(result)
}

fn unwrap(body: &[u8]) -> Result<Vec<u8>, ProtocolError> {
    let key_len = *body.first().ok_or(ProtocolError::InvalidJson)? as usize;
    if body.len() < key_len + 6 {
        return Err(ProtocolError::InvalidJson);
    }
    let len = body[5 + key_len] as usize;
    if body.len() < key_len + 6 + len {
        return Err(ProtocolError::InvalidJson);
    }
    Ok(body[key_len + 6..key_len + 6 + len].to_vec())
}

async fn read_frame(stream: &mut TcpStream, cipher: &mut Cipher) -> Result<Frame, ProtocolError> {
    let mut header = [0; FRAME_HEADER_SIZE];
    stream.read_exact(&mut header).await?;
    if header[0] != MAGIC {
        return Err(ProtocolError::InvalidMagic);
    }
    let length = u32::from_be_bytes(header[5..9].try_into().unwrap()) as usize;
    if length > MAX_PAYLOAD {
        return Err(ProtocolError::PayloadTooLarge(length));
    }
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await?;
    let encrypted = body.starts_with(&[0, 7, 1, 0xe0]);
    if encrypted {
        body = cipher.decrypt(&body)?;
    } else if header[1] == 0x14 {
        body = unwrap(&body)?;
    }
    Ok(Frame {
        outer: header[1],
        command: header[2],
        sequence: u16::from_be_bytes([header[3], header[4]]),
        body,
        encrypted,
    })
}

async fn write_frame(
    stream: &mut TcpStream,
    outer: u8,
    command: u8,
    sequence: u16,
    body: &[u8],
) -> Result<(), ProtocolError> {
    let mut header = [0; FRAME_HEADER_SIZE];
    header[0] = MAGIC;
    header[1] = outer;
    header[2] = command;
    header[3..5].copy_from_slice(&sequence.to_be_bytes());
    header[5..9].copy_from_slice(&(body.len() as u32).to_be_bytes());
    stream.write_all(&header).await?;
    stream.write_all(body).await?;
    stream.flush().await?;
    Ok(())
}

pub struct MiPlayServer {
    listener: TcpListener,
    identity: DeviceIdentity,
}

impl MiPlayServer {
    pub async fn bind(address: SocketAddr) -> Result<Self, std::io::Error> {
        Self::bind_with_identity(address, DeviceIdentity::default()).await
    }

    pub async fn bind_with_identity(
        address: SocketAddr,
        identity: DeviceIdentity,
    ) -> Result<Self, std::io::Error> {
        Ok(Self {
            listener: TcpListener::bind(address).await?,
            identity,
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr, std::io::Error> {
        self.listener.local_addr()
    }

    pub async fn accept(&self) -> Result<MiPlaySession, std::io::Error> {
        let (stream, peer) = self.listener.accept().await?;
        Ok(MiPlaySession {
            stream,
            peer,
            identity: self.identity.clone(),
        })
    }
}

pub struct MiPlaySession {
    stream: TcpStream,
    peer: SocketAddr,
    identity: DeviceIdentity,
}

impl MiPlaySession {
    pub fn peer_addr(&self) -> SocketAddr {
        self.peer
    }

    pub async fn run(mut self, events: mpsc::Sender<SessionEvent>) -> Result<(), ProtocolError> {
        tracing::info!(peer = %self.peer_addr(), "MiPlay session started");
        let local = self.stream.local_addr()?;
        let key = auth_key(local, self.peer);
        let mut cipher = Cipher::new(key);
        let mut own_challenge = [0; 16];
        rand::thread_rng().fill_bytes(&mut own_challenge);
        let own_challenge = hex::encode(own_challenge);
        let mut pending_auth: Option<(u16, Vec<u8>)> = None;
        let mut stream_keys = None;
        write_frame(
            &mut self.stream,
            0,
            DEVICE_ID,
            4,
            numeric_device_id(&self.identity.id).as_bytes(),
        )
        .await?;
        tracing::info!("MiPlay device identity announced");
        loop {
            let frame = match read_frame(&mut self.stream, &mut cipher).await {
                Ok(frame) => frame,
                Err(ProtocolError::Io(error))
                    if error.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    let _ = events.send(SessionEvent::Disconnected).await;
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
            let command_id = (u16::from(frame.outer) << 8) | u16::from(frame.command);
            tracing::info!(
                peer = %self.peer,
                outer = frame.outer,
                command = frame.command,
                sequence = frame.sequence,
                encrypted = frame.encrypted,
                body_bytes = frame.body.len(),
                "MiPlay control frame received"
            );
            match (frame.outer, frame.command) {
                (0, GET_VERSION) => {
                    write_frame(
                        &mut self.stream,
                        0,
                        GET_VERSION_ACK,
                        frame.sequence,
                        format!("{CONTROL_VERSION}\0").as_bytes(),
                    )
                    .await?;
                }
                (0, DEVICE_ID) => {
                    write_frame(
                        &mut self.stream,
                        0,
                        AUTH_ACK,
                        frame.sequence,
                        numeric_device_id(&self.identity.id).as_bytes(),
                    )
                    .await?;
                    tracing::debug!(
                        sequence = frame.sequence,
                        "MiPlay device identity acknowledged"
                    );
                }
                (0, AUTH_ACK) => {
                    write_frame(
                        &mut self.stream,
                        0,
                        NOTIFY,
                        5,
                        &[4, b'm', b'o', b'd', b'e', 3, 2],
                    )
                    .await?;
                    write_frame(
                        &mut self.stream,
                        0,
                        NOTIFY,
                        6,
                        &empty_media_info_notification(),
                    )
                    .await?;
                    write_frame(
                        &mut self.stream,
                        0,
                        NOTIFY,
                        7,
                        &[5, b's', b't', b'a', b't', 3, 0],
                    )
                    .await?;
                    tracing::debug!(
                        sequence = frame.sequence,
                        "MiPlay capabilities sent after AUTH_ACK"
                    );
                }
                (outer, SAFETY_INFO) if !frame.encrypted && (outer == 0 || outer == 0x14) => {
                    let info = br#"{"aesIvType":"2","aesKeyType":"1","authAlgorithmType":"4","authKeyType":"1","integrityType":"1","result":"0"}"#;
                    let response = if outer == 0x14 {
                        wrap("ack", info)?
                    } else {
                        info.to_vec()
                    };
                    write_frame(
                        &mut self.stream,
                        outer,
                        SAFETY_INFO_ACK,
                        frame.sequence,
                        &response,
                    )
                    .await?;
                    let payload = wrap(
                        "cmd",
                        format!(r#"{{"authMsg":"{own_challenge}"}}"#).as_bytes(),
                    )?;
                    let encrypted = cipher.encrypt(&payload)?;
                    write_frame(&mut self.stream, 0x14, SAFETY_AUTH, 0, &encrypted).await?;
                }
                (outer, SAFETY_AUTH) if outer == 0 || outer == 0x14 => {
                    let value = serde_json::from_slice::<Value>(&unwrap(&frame.body)?)
                        .map_err(|_| ProtocolError::InvalidJson)?;
                    let peer_challenge = value
                        .get("authMsg")
                        .and_then(Value::as_str)
                        .ok_or(ProtocolError::InvalidJson)?;
                    let ack = hmac_hex(&key, peer_challenge);
                    let response = format!(r#"{{"authMsgAck":"{ack}","result":"0"}}"#);
                    pending_auth = Some((frame.sequence, response.into_bytes()));
                }
                (outer, SAFETY_AUTH_ACK) if outer == 0 || outer == 0x14 => {
                    let value = serde_json::from_slice::<Value>(&unwrap(&frame.body)?)
                        .map_err(|_| ProtocolError::InvalidJson)?;
                    let received = value
                        .get("authMsgAck")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if received != hmac_hex(&key, &own_challenge) {
                        return Err(ProtocolError::InvalidJson);
                    }
                    if let Some((sequence, response)) = pending_auth.take() {
                        let wrapped = wrap("ack", &response)?;
                        let encrypted = cipher.encrypt(&wrapped)?;
                        write_frame(
                            &mut self.stream,
                            outer,
                            SAFETY_AUTH_ACK,
                            sequence,
                            &encrypted,
                        )
                        .await?;
                    }
                }
                (0, GET_DEVICE_INFO) => {
                    let body = format!(
                        r#"{{"deviceId":"{}","deviceName":"{}","model":"{}","deviceType":2}}"#,
                        self.identity.id, self.identity.name, self.identity.model
                    );
                    let encrypted = cipher.encrypt(body.as_bytes())?;
                    write_frame(
                        &mut self.stream,
                        0,
                        GET_DEVICE_INFO_ACK,
                        frame.sequence,
                        &encrypted,
                    )
                    .await?;
                }
                (0, SET_DEVICE_INFO) => {
                    let encrypted = cipher.encrypt(&[])?;
                    write_frame(
                        &mut self.stream,
                        0,
                        SET_DEVICE_INFO_ACK,
                        frame.sequence,
                        &encrypted,
                    )
                    .await?;
                }
                (0, GET_MIRROR_MODE) => {
                    let encrypted = cipher.encrypt(&[0, 0, 0, 0, 1])?;
                    write_frame(
                        &mut self.stream,
                        0,
                        GET_MIRROR_MODE_ACK,
                        frame.sequence,
                        &encrypted,
                    )
                    .await?;
                }
                (0, SET_MIRROR_KEY) => {
                    if let Ok(value) = serde_json::from_slice::<Value>(&unwrap(&frame.body)?) {
                        stream_keys = parse_stream_keys(&value);
                    }
                    let encrypted = cipher.encrypt(&[0])?;
                    write_frame(
                        &mut self.stream,
                        0,
                        SET_MIRROR_KEY_ACK,
                        frame.sequence,
                        &encrypted,
                    )
                    .await?;
                }
                (0, GET_VOLUME) => {
                    let encrypted = cipher.encrypt(&[50])?;
                    write_frame(
                        &mut self.stream,
                        0,
                        GET_VOLUME_ACK,
                        frame.sequence,
                        &encrypted,
                    )
                    .await?;
                }
                (0, HEARTBEAT) => {
                    let encrypted = cipher.encrypt(&[])?;
                    write_frame(
                        &mut self.stream,
                        0,
                        HEARTBEAT_ACK,
                        frame.sequence,
                        &encrypted,
                    )
                    .await?;
                }
                (0, GET_STATE) => {
                    let encrypted = cipher.encrypt(&[0, 0, 0, 0, 0])?;
                    write_frame(
                        &mut self.stream,
                        0,
                        GET_STATE_ACK,
                        frame.sequence,
                        &encrypted,
                    )
                    .await?;
                }
                (0, SET_PLAY_SOURCE) => {
                    let encrypted = cipher.encrypt(&[])?;
                    write_frame(
                        &mut self.stream,
                        0,
                        SET_PLAY_SOURCE_ACK,
                        frame.sequence,
                        &encrypted,
                    )
                    .await?;
                }
                (0, OPEN) if frame.encrypted => {
                    let url = String::from_utf8_lossy(&frame.body)
                        .trim_matches(char::from(0))
                        .to_string();
                    let (host, port) = parse_wfd_url(&url)?;
                    let keys = stream_keys.clone();
                    let tx = events.clone();
                    tokio::spawn(async move {
                        if let Err(error) = receive_rtsp(host, port, keys, tx).await {
                            tracing::warn!(%error, "MiPlay RTSP media session failed");
                        }
                    });
                    let encrypted = cipher.encrypt(&[0, 0, 0, 0, 0])?;
                    write_frame(&mut self.stream, 0, OPEN_ACK, frame.sequence, &encrypted).await?;
                }
                (0, PAUSE) => {
                    let encrypted = cipher.encrypt(&[])?;
                    write_frame(&mut self.stream, 0, PAUSE_ACK, frame.sequence, &encrypted).await?;
                }
                (0, RESUME) => {
                    let encrypted = cipher.encrypt(&[])?;
                    write_frame(&mut self.stream, 0, RESUME_ACK, frame.sequence, &encrypted)
                        .await?;
                }
                (0, SET_VOLUME) => {
                    let encrypted = cipher.encrypt(&[])?;
                    write_frame(
                        &mut self.stream,
                        0,
                        SET_VOLUME_ACK,
                        frame.sequence,
                        &encrypted,
                    )
                    .await?;
                }
                (0, SET_MEDIA_INFO) | (0, GET_MEDIA_INFO) => {
                    if frame.command == SET_MEDIA_INFO {
                        let encrypted = cipher.encrypt(&[])?;
                        write_frame(
                            &mut self.stream,
                            0,
                            SET_MEDIA_INFO_ACK,
                            frame.sequence,
                            &encrypted,
                        )
                        .await?;
                    } else {
                        let notification = cipher.encrypt(&empty_media_info_notification())?;
                        write_frame(&mut self.stream, 0, NOTIFY, 8, &notification).await?;
                    }
                }
                (0, SET_POSITION) => {
                    let encrypted = cipher.encrypt(&[])?;
                    write_frame(
                        &mut self.stream,
                        0,
                        SET_POSITION_ACK,
                        frame.sequence,
                        &encrypted,
                    )
                    .await?;
                }
                (0, SET_MEDIA_STATE) => {
                    let encrypted = cipher.encrypt(&[])?;
                    write_frame(
                        &mut self.stream,
                        0,
                        SET_MEDIA_STATE_ACK,
                        frame.sequence,
                        &encrypted,
                    )
                    .await?;
                }
                _ => {
                    tracing::debug!(
                        command = command_id,
                        sequence = frame.sequence,
                        "MiPlay control command"
                    );
                    let _ = events
                        .send(SessionEvent::Command {
                            command: command_id,
                            payload: frame.body.clone(),
                        })
                        .await;
                }
            }
            if matches!(frame.command, PAUSE | RESUME | SET_VOLUME) {
                tracing::debug!(
                    command = command_id,
                    sequence = frame.sequence,
                    "MiPlay playback command"
                );
                let _ = events
                    .send(SessionEvent::Command {
                        command: command_id,
                        payload: frame.body,
                    })
                    .await;
            }
        }
    }
}

fn parse_stream_keys(value: &Value) -> Option<StreamKeys> {
    fn key(value: &Value, name: &str) -> Option<[u8; 16]> {
        let bytes = value.get(name)?.as_str()?.as_bytes();
        bytes.len().eq(&16).then(|| bytes.try_into().ok()).flatten()
    }

    Some(StreamKeys {
        stream_key: key(value, "streamKey")?,
        stream_iv: key(value, "streamIV")?,
    })
}

fn numeric_device_id(id: &str) -> String {
    let digest = Md5::digest(id.as_bytes());
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    format!("{:014}", u64::from_be_bytes(bytes) % 100_000_000_000_000)
}

fn empty_media_info_notification() -> Vec<u8> {
    vec![
        0x0b, b'm', b'e', b'd', b'i', b'a', b'I', b'n', b'f', b'o', b'E', b'x', 0x16, 0x00, 0x00,
        0x00, 0x0c, 0x06, b'm', b'T', b'i', b't', b'l', b'e', 0x14, 0x00, 0x00, 0x00, 0x00,
    ]
}

fn parse_wfd_url(value: &str) -> Result<(String, u16), ProtocolError> {
    let authority = value
        .strip_prefix("wfd://")
        .ok_or_else(|| ProtocolError::InvalidUrl(value.into()))?
        .split('?')
        .next()
        .unwrap_or_default();
    let (host, port) = authority
        .rsplit_once(':')
        .ok_or_else(|| ProtocolError::InvalidUrl(value.into()))?;
    Ok((
        host.into(),
        port.parse()
            .map_err(|_| ProtocolError::InvalidUrl(value.into()))?,
    ))
}

async fn receive_rtsp(
    host: String,
    port: u16,
    keys: Option<StreamKeys>,
    events: mpsc::Sender<SessionEvent>,
) -> Result<(), ProtocolError> {
    let address = format!("{host}:{port}");
    let mut control = TcpStream::connect(&address).await?;
    let mut input = Vec::new();
    let mut data: Option<TcpStream> = None;
    let mut sent_options = false;
    loop {
        let message = read_rtsp(&mut control, &mut input).await?;
        let cseq = header(&message, "cseq").unwrap_or("0").to_string();
        if message.starts_with("OPTIONS") {
            let response = format!(
                "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nPublic: org.wfa.wfd1.0, GET_PARAMETER, SET_PARAMETER\r\n\r\n"
            );
            control.write_all(response.as_bytes()).await?;
            if !sent_options {
                sent_options = true;
                control.write_all(b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\nRequire: org.wfa.wfd1.0\r\nlib_version: audio-display-release2.1 2.1.5071614\r\n\r\n").await?;
            }
        } else if message.starts_with("GET_PARAMETER") {
            if message.contains("wfd_audio_codecs_v2") && data.is_none() {
                let image = TcpStream::connect(&address).await?;
                let audio = TcpStream::connect(&address).await?;
                let image_port = image.local_addr()?.port();
                let audio_port = audio.local_addr()?.port();
                tokio::spawn(async move {
                    let mut image = image;
                    let mut buffer = [0; 4096];
                    while image.read(&mut buffer).await.unwrap_or(0) > 0 {}
                });
                data = Some(audio);
                let body = "wfd_audio_codecs_v2: 15 3 3\r\nwfd_video_formats: none\r\nwfd_client_rtp_ports: RTP/AVP/TCP;interleaved mode=play\r\n";
                control
                    .write_all(
                        format!(
                            "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nContent-Length: {}\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await?;
                let setup = format!(
                    "SETUP rtsp://{host}/wfd1.0/streamid=0 RTSP/1.0\r\nCSeq: 2\r\nTransport: RTP/AVP/TCP;interleaved=0-1\r\nMultiPort: image_port={image_port};multi_port={audio_port}\r\n\r\n"
                );
                control.write_all(setup.as_bytes()).await?;
            } else {
                control
                    .write_all(format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n\r\n").as_bytes())
                    .await?;
            }
        } else if message.starts_with("SET_PARAMETER") {
            control
                .write_all(format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n\r\n").as_bytes())
                .await?;
            if message.contains("wfd_trigger_method: SETUP") {
                control.write_all(format!("SETUP rtsp://{host}/wfd1.0/streamid=0 RTSP/1.0\r\nCSeq: 2\r\nTransport: RTP/AVP/TCP;interleaved=0-1\r\n\r\n").as_bytes()).await?;
            }
        } else if message.starts_with("PLAY") {
            control
                .write_all(format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n\r\n").as_bytes())
                .await?;
            if let Some(audio) = data.take() {
                receive_rtp(audio, keys, events).await?;
                return Ok(());
            }
        } else if message.starts_with("PAUSE") {
            control
                .write_all(format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n\r\n").as_bytes())
                .await?;
        } else if message.starts_with("RTSP/1.0") && message.contains("CSeq: 2") {
            control.write_all(format!("PLAY rtsp://{host}/wfd1.0/streamid=0 RTSP/1.0\r\nCSeq: 3\r\nSession: 1\r\n\r\n").as_bytes()).await?;
        }
    }
}

async fn read_rtsp(stream: &mut TcpStream, input: &mut Vec<u8>) -> Result<String, ProtocolError> {
    loop {
        if let Some(end) = input.windows(4).position(|w| w == b"\r\n\r\n") {
            let header_len = end + 4;
            let header_text = std::str::from_utf8(&input[..header_len])
                .map_err(|_| ProtocolError::InvalidRtsp)?;
            let content_length = header(header_text, "content-length")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(0);
            let total = header_len
                .checked_add(content_length)
                .ok_or(ProtocolError::InvalidRtsp)?;
            if input.len() < total {
                let mut buffer = [0; 4096];
                let read = stream.read(&mut buffer).await?;
                if read == 0 {
                    return Err(ProtocolError::Truncated);
                }
                input.extend_from_slice(&buffer[..read]);
                continue;
            }
            let bytes: Vec<u8> = input.drain(..total).collect();
            return String::from_utf8(bytes).map_err(|_| ProtocolError::InvalidRtsp);
        }
        let mut buffer = [0; 4096];
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Err(ProtocolError::Truncated);
        }
        input.extend_from_slice(&buffer[..read]);
        if input.len() > 64 * 1024 {
            return Err(ProtocolError::InvalidRtsp);
        }
    }
}

fn header<'a>(message: &'a str, name: &str) -> Option<&'a str> {
    message.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then_some(value.trim())
    })
}

async fn receive_rtp(
    mut stream: TcpStream,
    keys: Option<StreamKeys>,
    events: mpsc::Sender<SessionEvent>,
) -> Result<(), ProtocolError> {
    let mut demuxer = TsDemuxer::new(keys);
    loop {
        let mut marker = [0; 4];
        stream.read_exact(&mut marker).await?;
        if marker[0] != b'$' {
            return Err(ProtocolError::InvalidRtp);
        }
        let length = u16::from_be_bytes([marker[2], marker[3]]) as usize;
        if !(12..=65_535).contains(&length) {
            return Err(ProtocolError::InvalidRtp);
        }
        let mut packet = vec![0; length];
        stream.read_exact(&mut packet).await?;
        let payload = rtp_payload(&packet)?;
        for ts in payload.chunks_exact(188) {
            for aac in demuxer.push(ts)? {
                let pcm = decode_aac(&aac)?;
                let pcm_bytes = pcm.len();
                events
                    .send(SessionEvent::Audio(PcmAudioFrame {
                        sequence: 0,
                        timestamp: 0,
                        sample_rate: 48_000,
                        channels: 2,
                        pcm,
                    }))
                    .await
                    .map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;
                tracing::trace!(pcm_bytes, "MiPlay decoded PCM frame");
            }
        }
    }
}

fn rtp_payload(packet: &[u8]) -> Result<&[u8], ProtocolError> {
    if packet.len() < 12 || packet[0] >> 6 != 2 {
        return Err(ProtocolError::InvalidRtp);
    }
    let mut offset = 12 + usize::from(packet[0] & 0x0f) * 4;
    if packet[0] & 0x10 != 0 {
        if packet.len() < offset + 4 {
            return Err(ProtocolError::InvalidRtp);
        }
        offset += 4 + usize::from(u16::from_be_bytes([packet[offset + 2], packet[offset + 3]])) * 4;
    }
    let end = if packet[0] & 0x20 != 0 {
        packet
            .len()
            .checked_sub(*packet.last().ok_or(ProtocolError::InvalidRtp)? as usize)
            .ok_or(ProtocolError::InvalidRtp)?
    } else {
        packet.len()
    };
    packet.get(offset..end).ok_or(ProtocolError::InvalidRtp)
}

struct TsDemuxer {
    pes: Vec<u8>,
    keys: Option<StreamKeys>,
}

impl TsDemuxer {
    fn new(keys: Option<StreamKeys>) -> Self {
        Self {
            pes: Vec::new(),
            keys,
        }
    }

    fn push(&mut self, packet: &[u8]) -> Result<Vec<Vec<u8>>, ProtocolError> {
        if packet.len() != 188 || packet[0] != 0x47 {
            return Ok(Vec::new());
        }
        let pid = (u16::from(packet[1] & 0x1f) << 8) | u16::from(packet[2]);
        if pid != 0x1100 {
            return Ok(Vec::new());
        }
        let adaptation = (packet[3] >> 4) & 3;
        if adaptation == 0 || adaptation == 2 {
            return Ok(Vec::new());
        }
        let offset = if adaptation == 3 {
            5 + packet[4] as usize
        } else {
            4
        };
        if offset >= packet.len() {
            return Ok(Vec::new());
        }
        let mut result = Vec::new();
        if packet[1] & 0x40 != 0 && !self.pes.is_empty() {
            if let Some(payload) = self.finish()? {
                result.push(payload);
            }
        }
        self.pes.extend_from_slice(&packet[offset..]);
        if self.pes.len() >= 6 {
            let declared = u16::from_be_bytes([self.pes[4], self.pes[5]]) as usize;
            if declared > 0 && self.pes.len() >= declared + 6 {
                if let Some(payload) = self.finish()? {
                    result.push(payload);
                }
            }
        }
        Ok(result)
    }

    fn finish(&mut self) -> Result<Option<Vec<u8>>, ProtocolError> {
        let mut pes = std::mem::take(&mut self.pes);
        if pes.len() < 9 || pes[..4] != [0, 0, 1, 0xc0] {
            return Ok(None);
        }
        let declared = u16::from_be_bytes([pes[4], pes[5]]) as usize;
        if declared > 0 && declared + 6 < pes.len() {
            pes.truncate(declared + 6);
        }
        let offset = 9 + pes[8] as usize;
        if offset >= pes.len() {
            return Ok(None);
        }
        let mut payload = pes[offset..].to_vec();
        if !is_adts(&payload) {
            if let Some(keys) = self.keys.as_mut() {
                let length = payload.len() / 16 * 16;
                if length > 0 {
                    let decryptor =
                        AesCbcDecrypt::new_from_slices(&keys.stream_key, &keys.stream_iv)
                            .map_err(|_| ProtocolError::InvalidEncryptedBody)?;
                    decryptor
                        .decrypt_padded_mut::<NoPadding>(&mut payload[..length])
                        .map_err(|_| ProtocolError::InvalidEncryptedBody)?;
                }
            }
        }
        Ok(Some(payload))
    }
}

fn is_adts(data: &[u8]) -> bool {
    data.len() >= 7 && data[0] == 0xff && data[1] & 0xf6 == 0xf0
}

fn decode_aac(data: &[u8]) -> Result<Vec<u8>, ProtocolError> {
    use std::io::Cursor;
    use symphonia::core::{
        audio::{AudioBufferRef, Signal},
        codecs::DecoderOptions,
        formats::FormatOptions,
        io::MediaSourceStream,
        probe::Hint,
    };
    let mut hint = Hint::new();
    hint.with_extension("aac");
    let source = MediaSourceStream::new(Box::new(Cursor::new(data.to_vec())), Default::default());
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            source,
            &FormatOptions::default(),
            &Default::default(),
        )
        .map_err(|error| ProtocolError::Aac(error.to_string()))?;
    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| ProtocolError::Aac("AAC track missing".into()))?;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|error| ProtocolError::Aac(error.to_string()))?;
    let packet = format
        .next_packet()
        .map_err(|error| ProtocolError::Aac(error.to_string()))?;
    let decoded = decoder
        .decode(&packet)
        .map_err(|error| ProtocolError::Aac(error.to_string()))?;
    let mut output = Vec::new();
    match decoded {
        AudioBufferRef::S16(buffer) => {
            for frame in 0..buffer.frames() {
                for channel in 0..buffer.spec().channels.count() {
                    output.extend_from_slice(&buffer.chan(channel)[frame].to_le_bytes());
                }
            }
        }
        other => {
            let spec = *other.spec();
            let mut sample_buffer =
                symphonia::core::audio::SampleBuffer::<i16>::new(other.capacity() as u64, spec);
            sample_buffer.copy_interleaved_ref(other);
            for sample in sample_buffer.samples() {
                output.extend_from_slice(&sample.to_le_bytes());
            }
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_key_matches_protocol_shape() {
        let key = auth_key(
            "192.0.2.1:8899".parse().unwrap(),
            "192.0.2.2:4000".parse().unwrap(),
        );
        assert_eq!(key.len(), 16);
    }

    #[test]
    fn parses_wfd_url() {
        assert_eq!(
            parse_wfd_url("wfd://192.0.2.2:7236?x=1").unwrap(),
            ("192.0.2.2".into(), 7236)
        );
    }
}
