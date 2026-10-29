use std::{
    collections::VecDeque,
    net::{IpAddr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::{Duration, UNIX_EPOCH},
};

use aes::{
    Aes128,
    cipher::{BlockEncrypt, KeyInit},
};
use alac_encoder::{AlacEncoder, FormatDescription};
use base64::{Engine, engine::general_purpose::STANDARD};
use md5::{Digest, Md5};
use rand::RngCore;
use rsa::{Oaep, RsaPublicKey};
use sha1::Sha1;
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
};

const RTP_HEADER_LEN: usize = 12;
type RtpBacklog = Arc<Mutex<VecDeque<(u16, Vec<u8>)>>>;

#[derive(Debug, Error)]
pub enum RaopError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid RTSP response: {0}")]
    InvalidResponse(String),
    #[error("RAOP request failed with status {status}: {reason}")]
    RequestFailed { status: u16, reason: String },
    #[error("RAOP response is missing {0} header")]
    MissingHeader(&'static str),
    #[error("audio payload is too large for one RTP packet")]
    AudioPayloadTooLarge,
    #[error("unsupported audio format: {sample_rate} Hz, {channels} channels")]
    UnsupportedAudioFormat { sample_rate: u32, channels: u8 },
    #[error("PCM payload length {0} is not aligned to the sample format")]
    InvalidPcmLength(usize),
    #[error("RAOP encryption setup failed: {0}")]
    Crypto(String),
    #[error("mDNS discovery failed: {0}")]
    Discovery(String),
}

pub fn discover_raop(timeout: Duration) -> Result<Vec<RaopEndpoint>, RaopError> {
    let daemon =
        mdns_sd::ServiceDaemon::new().map_err(|error| RaopError::Discovery(error.to_string()))?;
    let events = daemon
        .browse("_raop._tcp.local.")
        .map_err(|error| RaopError::Discovery(error.to_string()))?;
    let deadline = std::time::Instant::now() + timeout;
    let mut endpoints = Vec::new();
    while let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) {
        let Ok(event) = events.recv_timeout(remaining) else {
            break;
        };
        if let mdns_sd::ServiceEvent::ServiceResolved(info) = event {
            for address in &info.addresses {
                let endpoint = RaopEndpoint {
                    address: SocketAddr::new(address.to_ip_addr(), info.port),
                    name: info.fullname.clone(),
                };
                if !endpoints.contains(&endpoint) {
                    endpoints.push(endpoint);
                }
            }
        }
    }
    let _ = daemon.shutdown();
    Ok(endpoints)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RaopEndpoint {
    pub address: SocketAddr,
    pub name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioPacket {
    pub sequence: u16,
    pub timestamp: u32,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PcmAudioFrame {
    pub sequence: u16,
    pub timestamp: u32,
    pub sample_rate: u32,
    pub channels: u8,
    /// Interleaved signed 16-bit PCM, little-endian.
    pub pcm: Vec<u8>,
}

pub struct PcmToRaop {
    encoder: AlacEncoder,
    input: FormatDescription,
    sample_rate: u32,
    channels: u8,
    samples_per_packet: usize,
    pending: Vec<u8>,
    next_sequence: u16,
    next_timestamp: u32,
}

impl PcmToRaop {
    pub fn new(sample_rate: u32, channels: u8) -> Result<Self, RaopError> {
        if channels == 0 || channels > 2 || sample_rate == 0 {
            return Err(RaopError::UnsupportedAudioFormat {
                sample_rate,
                channels,
            });
        }
        let output = FormatDescription::alac(sample_rate as f64, 352, channels as u32);
        let input = FormatDescription::pcm::<i16>(sample_rate as f64, channels as u32);
        Ok(Self {
            encoder: AlacEncoder::new(&output),
            input,
            sample_rate,
            channels,
            samples_per_packet: 352,
            pending: Vec::new(),
            next_sequence: 0,
            next_timestamp: 0,
        })
    }

    pub fn push(&mut self, frame: PcmAudioFrame) -> Result<Vec<AudioPacket>, RaopError> {
        if frame.sample_rate != self.sample_rate || frame.channels != self.channels {
            return Err(RaopError::UnsupportedAudioFormat {
                sample_rate: frame.sample_rate,
                channels: frame.channels,
            });
        }
        let bytes_per_sample = self.channels as usize * 2;
        if frame.pcm.len() % bytes_per_sample != 0 {
            return Err(RaopError::InvalidPcmLength(frame.pcm.len()));
        }
        if self.pending.is_empty() {
            self.next_sequence = frame.sequence;
            self.next_timestamp = frame.timestamp;
        }
        self.pending.extend_from_slice(&frame.pcm);
        let packet_bytes = self.samples_per_packet * bytes_per_sample;
        let mut packets = Vec::new();
        while self.pending.len() >= packet_bytes {
            let pcm: Vec<u8> = self.pending.drain(..packet_bytes).collect();
            let mut encoded = vec![0; pcm.len() + self.input.max_packet_size()];
            let size = self.encoder.encode(&self.input, &pcm, &mut encoded);
            encoded.truncate(size);
            packets.push(AudioPacket {
                sequence: self.next_sequence,
                timestamp: self.next_timestamp,
                payload: encoded,
            });
            self.next_sequence = self.next_sequence.wrapping_add(1);
            self.next_timestamp = self
                .next_timestamp
                .wrapping_add(self.samples_per_packet as u32);
        }
        Ok(packets)
    }

    pub fn buffered_samples(&self) -> usize {
        self.pending.len() / (self.channels as usize * 2)
    }
}

pub struct RaopClient {
    rtsp: TcpStream,
    session: String,
    cseq: u32,
    ssrc: u32,
    audio: Arc<UdpSocket>,
    control: Arc<UdpSocket>,
    _timing: Arc<UdpSocket>,
    remote_audio: SocketAddr,
    remote_control: SocketAddr,
    remote_timing: SocketAddr,
    next_sequence: u16,
    next_timestamp: u32,
    aes_key: [u8; 16],
    aes_iv: [u8; 16],
    client_instance: String,
    password: Option<String>,
    auth: Option<(String, String)>,
    backlog: RtpBacklog,
    running: Arc<AtomicBool>,
    stream_timestamp: Arc<AtomicU32>,
    sample_rate: u32,
}

impl RaopClient {
    pub async fn connect(endpoint: &RaopEndpoint) -> Result<Self, RaopError> {
        Self::connect_with_password(endpoint, 44_100, 2, None).await
    }

    pub async fn connect_with_audio_format(
        endpoint: &RaopEndpoint,
        sample_rate: u32,
        channels: u8,
    ) -> Result<Self, RaopError> {
        Self::connect_with_password(endpoint, sample_rate, channels, None).await
    }

    pub async fn connect_with_password(
        endpoint: &RaopEndpoint,
        sample_rate: u32,
        channels: u8,
        password: Option<&str>,
    ) -> Result<Self, RaopError> {
        if channels == 0 || channels > 2 || sample_rate == 0 {
            return Err(RaopError::UnsupportedAudioFormat {
                sample_rate,
                channels,
            });
        }
        let rtsp = TcpStream::connect(endpoint.address).await?;
        let local_ip = rtsp.local_addr()?.ip();
        let audio = UdpSocket::bind((local_ip, 0)).await?;
        let control = UdpSocket::bind((local_ip, 0)).await?;
        let timing = UdpSocket::bind((local_ip, 0)).await?;
        let audio_port = audio.local_addr()?.port();
        let control_port = control.local_addr()?.port();
        let timing_port = timing.local_addr()?.port();
        let session = format!("{:08x}", rand_seed());
        let mut aes_key = [0u8; 16];
        let mut aes_iv = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut aes_key);
        rand::thread_rng().fill_bytes(&mut aes_iv);
        let client_instance = format!("{:016X}", rand_seed() as u64);
        let mut client = Self {
            rtsp,
            session,
            cseq: 1,
            ssrc: rand_seed(),
            audio: Arc::new(audio),
            control: Arc::new(control),
            _timing: Arc::new(timing),
            remote_audio: SocketAddr::new(endpoint.address.ip(), 0),
            remote_control: SocketAddr::new(endpoint.address.ip(), 0),
            remote_timing: SocketAddr::new(endpoint.address.ip(), 0),
            next_sequence: 0,
            next_timestamp: 0,
            aes_key,
            aes_iv,
            client_instance,
            password: password.map(str::to_owned),
            auth: None,
            backlog: Arc::new(Mutex::new(VecDeque::with_capacity(512))),
            running: Arc::new(AtomicBool::new(true)),
            stream_timestamp: Arc::new(AtomicU32::new(0)),
            sample_rate,
        };

        client.request("OPTIONS", "", &[]).await?;
        let local_ip_text = match local_ip {
            IpAddr::V4(ip) => format!("IN IP4 {ip}"),
            IpAddr::V6(ip) => format!("IN IP6 {ip}"),
        };
        let encrypted_key = rsa_encrypt_key(&client.aes_key)?;
        let key = STANDARD
            .encode(encrypted_key)
            .trim_end_matches('=')
            .to_owned();
        let iv = STANDARD
            .encode(client.aes_iv)
            .trim_end_matches('=')
            .to_owned();
        let sdp = format!(
            "v=0\r\no=iTunes {} 0 {}\r\ns=iTunes\r\nc={}\r\nt=0 0\r\n\
m=audio 0 RTP/AVP 96\r\na=rtpmap:96 AppleLossless\r\na=fmtp:96 352 0 16 40 10 14 {} 255 0 0 {}\r\n\
a=rsaaeskey:{}\r\na=aesiv:{}\r\n",
            client.session, local_ip_text, local_ip_text, channels, sample_rate, key, iv
        );
        let mut challenge_bytes = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut challenge_bytes);
        let challenge = STANDARD
            .encode(challenge_bytes)
            .trim_end_matches('=')
            .to_owned();
        let announce_response = client
            .request(
                "ANNOUNCE",
                &sdp,
                &[
                    ("Content-Type", "application/sdp"),
                    ("Apple-Challenge", &challenge),
                ],
            )
            .await?;
        if let Some(response) = announce_response.headers.get("apple-response") {
            let decoded = STANDARD.decode(format!("{response}==")).map_err(|error| {
                RaopError::InvalidResponse(format!("invalid Apple-Response: {error}"))
            })?;
            if decoded.is_empty() {
                return Err(RaopError::InvalidResponse("empty Apple-Response".into()));
            }
        }
        let transport = format!(
            "RTP/AVP/UDP;unicast;mode=record;timing_port={};control_port={};",
            timing_port, control_port
        );
        let response = client
            .request(
                "SETUP",
                "",
                &[("Transport", &format!("{transport}client_port={audio_port}"))],
            )
            .await?;
        let (remote_audio, remote_control, remote_timing) = parse_transport(
            response
                .headers
                .get("transport")
                .ok_or(RaopError::MissingHeader("Transport"))?,
            endpoint.address.ip(),
        )?;
        client.remote_audio = remote_audio;
        client.remote_control = remote_control;
        client.remote_timing = remote_timing;
        client.audio.connect(remote_audio).await?;
        client.control.connect(remote_control).await?;
        if remote_timing.port() != 0 {
            client._timing.connect(remote_timing).await?;
        }
        client
            .request(
                "RECORD",
                "",
                &[("Range", "npt=0-"), ("RTP-Info", "seq=0;rtptime=0")],
            )
            .await?;
        client.start_rtp_workers();
        Ok(client)
    }

    pub async fn send_alac(&mut self, packet: &AudioPacket) -> Result<(), RaopError> {
        if packet.payload.len() > u16::MAX as usize {
            return Err(RaopError::AudioPayloadTooLarge);
        }
        let mut rtp = Vec::with_capacity(RTP_HEADER_LEN + packet.payload.len());
        rtp.extend_from_slice(&[0x80, 0x60]);
        rtp.extend_from_slice(&packet.sequence.to_be_bytes());
        rtp.extend_from_slice(&packet.timestamp.to_be_bytes());
        rtp.extend_from_slice(&self.ssrc.to_be_bytes());
        let mut payload = packet.payload.clone();
        encrypt_rtp_payload(&mut payload, &self.aes_key, &self.aes_iv);
        rtp.extend_from_slice(&payload);
        if let Ok(mut backlog) = self.backlog.lock() {
            backlog.push_back((packet.sequence, rtp.clone()));
            while backlog.len() > 512 {
                backlog.pop_front();
            }
        }
        tracing::trace!(
            sequence = packet.sequence,
            timestamp = packet.timestamp,
            payload_bytes = packet.payload.len(),
            "RAOP RTP audio packet"
        );
        self.audio.send(&rtp).await?;
        self.next_sequence = packet.sequence.wrapping_add(1);
        self.next_timestamp = packet.timestamp;
        self.stream_timestamp
            .store(packet.timestamp, Ordering::Relaxed);
        Ok(())
    }

    pub async fn send_pcm(
        &mut self,
        bridge: &mut PcmToRaop,
        frame: PcmAudioFrame,
    ) -> Result<usize, RaopError> {
        let packets = bridge.push(frame)?;
        let count = packets.len();
        for packet in packets {
            self.send_alac(&packet).await?;
        }
        Ok(count)
    }

    pub async fn pause(&mut self) -> Result<(), RaopError> {
        self.request(
            "FLUSH",
            "",
            &[(
                "RTP-Info",
                &format!("seq={},rtptime={}", self.next_sequence, self.next_timestamp),
            )],
        )
        .await?;
        Ok(())
    }

    pub async fn resume(&mut self) -> Result<(), RaopError> {
        self.request("RECORD", "", &[("Range", "npt=0-")]).await?;
        Ok(())
    }

    pub async fn set_volume(&mut self, volume: f32) -> Result<(), RaopError> {
        let volume = volume.clamp(-30.0, 0.0);
        self.request(
            "SET_PARAMETER",
            &format!("volume: {volume:.2}\r\n"),
            &[("Content-Type", "text/parameters")],
        )
        .await?;
        Ok(())
    }

    pub fn volume_db(volume: u8) -> f32 {
        if volume == 0 {
            -30.0
        } else {
            -30.0 + 30.0 * (volume as f32 / 100.0)
        }
    }

    pub async fn stop(mut self) -> Result<(), RaopError> {
        self.running.store(false, Ordering::Relaxed);
        self.request("TEARDOWN", "", &[]).await?;
        Ok(())
    }

    fn start_rtp_workers(&self) {
        let control = Arc::clone(&self.control);
        let backlog = Arc::clone(&self.backlog);
        let running = Arc::clone(&self.running);
        let stream_timestamp = Arc::clone(&self.stream_timestamp);
        let sample_rate = self.sample_rate;
        tokio::spawn(async move {
            let mut buffer = [0u8; 64];
            let mut sync_interval = tokio::time::interval(Duration::from_secs(1));
            while running.load(Ordering::Relaxed) {
                tokio::select! {
                    result = control.recv(&mut buffer) => {
                        let Ok(size) = result else { break };
                        if size < 8 {
                            continue;
                        }
                        let sequence = u16::from_be_bytes([buffer[4], buffer[5]]);
                        let count = u16::from_be_bytes([buffer[6], buffer[7]]);
                        for offset in 0..count {
                            let missing = sequence.wrapping_add(offset);
                            let packet = backlog.lock().ok().and_then(|items| {
                                items.iter().find(|(seq, _)| *seq == missing)
                                    .map(|(_, packet)| packet.clone())
                            });
                            if let Some(mut packet) = packet {
                                packet[1] = 0xd6;
                                tracing::debug!(sequence = missing, "RAOP RTP packet retransmit");
                                let _ = control.send(&packet).await;
                            }
                        }
                    }
                    _ = sync_interval.tick() => {
                        let timestamp = stream_timestamp.load(Ordering::Relaxed);
                        tracing::trace!(timestamp, "RAOP RTP sync packet");
                        let _ = control.send(&build_sync_packet(timestamp, sample_rate, false)).await;
                    }
                }
            }
        });

        let timing = Arc::clone(&self._timing);
        let remote_timing = self.remote_timing;
        let running = Arc::clone(&self.running);
        tokio::spawn(async move {
            if remote_timing.port() == 0 {
                return;
            }
            let mut request = [0u8; 32];
            while running.load(Ordering::Relaxed) {
                let Ok(size) = timing.recv(&mut request).await else {
                    break;
                };
                if size != 32 {
                    continue;
                }
                let mut response = request;
                response[1] = 0xd3;
                response[8..16].copy_from_slice(&request[24..32]);
                let now = ntp_now();
                tracing::trace!("RAOP RTP timing response");
                response[16..24].copy_from_slice(&now);
                response[24..32].copy_from_slice(&now);
                let _ = timing.send(&response).await;
            }
        });
    }

    async fn request(
        &mut self,
        method: &str,
        body: &str,
        headers: &[(&str, &str)],
    ) -> Result<Response, RaopError> {
        let mut request = format!(
            "{method} rtsp://mi2play/streamid=0 RTSP/1.0\r\nCSeq: {}\r\nSession: {}\r\nUser-Agent: Mi2Play/0.1\r\n",
            self.cseq, self.session
        );
        request.push_str(&format!(
            "DACP-ID: {}\r\nActive-Remote: {}\r\nClient-Instance: {}\r\n",
            self.client_instance, self.session, self.client_instance
        ));
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        if !body.is_empty() {
            request.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        request.push_str("\r\n");
        request.push_str(body);
        self.rtsp.write_all(request.as_bytes()).await?;
        self.cseq += 1;
        let response = read_response(&mut self.rtsp).await?;
        tracing::debug!(
            method,
            cseq = self.cseq - 1,
            status = response.status,
            "RAOP RTSP response"
        );
        if response.status == 401 {
            let Some(password) = self.password.as_deref() else {
                return Err(RaopError::RequestFailed {
                    status: response.status,
                    reason: "receiver requires a password".into(),
                });
            };
            let Some(challenge) = response.headers.get("www-authenticate") else {
                return Err(RaopError::InvalidResponse(
                    "401 response missing WWW-Authenticate".into(),
                ));
            };
            self.auth = parse_digest_challenge(challenge);
            let Some((realm, nonce)) = self.auth.clone() else {
                return Err(RaopError::InvalidResponse(
                    "unsupported WWW-Authenticate challenge".into(),
                ));
            };
            let uri = if method == "OPTIONS" {
                "*"
            } else {
                "rtsp://mi2play/streamid=0"
            };
            let username = if realm.eq_ignore_ascii_case("raop") {
                "iTunes"
            } else {
                "AirPlay"
            };
            let ha1 = md5_hex(format!("{username}:{realm}:{password}").as_bytes());
            let ha2 = md5_hex(format!("{method}:{uri}").as_bytes());
            let response_hash = md5_hex(format!("{ha1}:{nonce}:{ha2}").as_bytes());
            let authorization = format!(
                "Digest username=\"{username}\", realm=\"{realm}\", nonce=\"{nonce}\", uri=\"{uri}\", response=\"{response_hash}\""
            );
            let mut retry = format!(
                "{method} {uri} RTSP/1.0\r\nCSeq: {}\r\nSession: {}\r\nUser-Agent: Mi2Play/0.1\r\nAuthorization: {authorization}\r\n",
                self.cseq, self.session
            );
            for (name, value) in headers {
                retry.push_str(&format!("{name}: {value}\r\n"));
            }
            if !body.is_empty() {
                retry.push_str(&format!("Content-Length: {}\r\n", body.len()));
            }
            retry.push_str("\r\n");
            retry.push_str(body);
            self.rtsp.write_all(retry.as_bytes()).await?;
            self.cseq += 1;
            let response = read_response(&mut self.rtsp).await?;
            tracing::debug!(
                method,
                cseq = self.cseq - 1,
                status = response.status,
                "RAOP RTSP authenticated response"
            );
            if !(200..300).contains(&response.status) {
                return Err(RaopError::RequestFailed {
                    status: response.status,
                    reason: response.reason,
                });
            }
            return Ok(response);
        }
        if !(200..300).contains(&response.status) {
            return Err(RaopError::RequestFailed {
                status: response.status,
                reason: response.reason,
            });
        }
        Ok(response)
    }
}

#[derive(Debug)]
struct Response {
    status: u16,
    reason: String,
    headers: std::collections::HashMap<String, String>,
}

async fn read_response(stream: &mut TcpStream) -> Result<Response, RaopError> {
    let mut buffer = Vec::new();
    let header_end;
    loop {
        let mut chunk = [0; 1024];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(RaopError::InvalidResponse("connection closed".into()));
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            header_end = end + 4;
            break;
        }
        if buffer.len() > 64 * 1024 {
            return Err(RaopError::InvalidResponse("headers too large".into()));
        }
    }
    let text = std::str::from_utf8(&buffer[..header_end])
        .map_err(|_| RaopError::InvalidResponse("headers are not UTF-8".into()))?;
    let mut lines = text.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| RaopError::InvalidResponse("missing status".into()))?;
    let mut status_parts = status_line.splitn(3, ' ');
    if status_parts.next() != Some("RTSP/1.0") {
        return Err(RaopError::InvalidResponse(status_line.into()));
    }
    let status = status_parts
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| RaopError::InvalidResponse(status_line.into()))?;
    let reason = status_parts.next().unwrap_or_default().into();
    let mut headers = std::collections::HashMap::new();
    for line in lines.filter(|line| !line.is_empty()) {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().into());
        }
    }
    Ok(Response {
        status,
        reason,
        headers,
    })
}

fn parse_digest_challenge(value: &str) -> Option<(String, String)> {
    let value = value.strip_prefix("Digest ")?;
    let mut realm = None;
    let mut nonce = None;
    for item in value.split(',') {
        let (name, value) = item.trim().split_once('=')?;
        let value = value.trim().trim_matches('"').to_owned();
        match name {
            "realm" => realm = Some(value),
            "nonce" => nonce = Some(value),
            _ => {}
        }
    }
    Some((realm?, nonce?))
}

fn md5_hex(value: &[u8]) -> String {
    let mut digest = Md5::new();
    digest.update(value);
    hex::encode(digest.finalize())
}

fn parse_transport(
    value: &str,
    ip: IpAddr,
) -> Result<(SocketAddr, SocketAddr, SocketAddr), RaopError> {
    let mut audio = None;
    let mut control = None;
    let mut timing = None;
    for item in value.split(';') {
        let Some((key, port)) = item.split_once('=') else {
            continue;
        };
        match key.trim().to_ascii_lowercase().as_str() {
            "server_port" => audio = port.parse::<u16>().ok(),
            "control_port" => control = port.parse::<u16>().ok(),
            "timing_port" => timing = port.parse::<u16>().ok(),
            _ => {}
        }
    }
    let audio = audio.ok_or(RaopError::MissingHeader("Transport server_port"))?;
    let control = control.ok_or(RaopError::MissingHeader("Transport control_port"))?;
    let timing = timing.unwrap_or(0);
    Ok((
        SocketAddr::new(ip, audio),
        SocketAddr::new(ip, control),
        SocketAddr::new(ip, timing),
    ))
}

fn ntp_now() -> [u8; 8] {
    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = (now.as_secs() + 2_208_988_800) as u32;
    let fraction = ((u64::from(now.subsec_nanos()) << 32) / 1_000_000_000) as u32;
    let mut result = [0; 8];
    result[..4].copy_from_slice(&seconds.to_be_bytes());
    result[4..].copy_from_slice(&fraction.to_be_bytes());
    result
}

fn build_sync_packet(timestamp: u32, sample_rate: u32, first: bool) -> [u8; 20] {
    let mut packet = [0u8; 20];
    packet[0] = if first { 0x90 } else { 0x80 };
    packet[1] = 0xd4;
    packet[2..4].copy_from_slice(&7u16.to_be_bytes());
    let ntp = if sample_rate == 0 {
        0
    } else {
        (u64::from(timestamp) << 32) / u64::from(sample_rate)
    };
    packet[4..12].copy_from_slice(&ntp.to_be_bytes());
    packet[12..16].copy_from_slice(&timestamp.to_be_bytes());
    packet[16..20].copy_from_slice(&0u32.to_be_bytes());
    packet
}

fn rand_seed() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos()
}

fn rsa_encrypt_key(key: &[u8; 16]) -> Result<Vec<u8>, RaopError> {
    const MODULUS: &str = "59dE8qLieItsH1WgjrcFRKj6eUWqi+bGLOX1HL3U3GhC/j0Qg90u3sG/1CUtwC5vOYvfDmFI6oSFXi5ELabWJmT2dKHzBJKa3k9ok+8t9ucRqMd6DZHJ2YCCLlDRKSKv6kDqnw4UwPdpOMXziC/AMj3Z/lUVX1G7WSHCAWKf1zNS1eLvqr+boEjXuBOitnZ/bDzPHrTOZz0Dew0uowxf/+sG+NCK3eQJVxqcaJ/vEHKIVd2M+5qL71yJQ+87X6oV3eaYvt3zWZYD6z5vYTcrtij2VZ9Zmni/UAaHqn9JdsBWLUEpVviYnhimNVvYFZeCXg/IdTQ+x4IRdiXNv5hEew==";
    let modulus = STANDARD
        .decode(MODULUS)
        .map_err(|error| RaopError::Crypto(error.to_string()))?;
    let public = RsaPublicKey::new(
        rsa::BigUint::from_bytes_be(&modulus),
        rsa::BigUint::from(65_537u32),
    )
    .map_err(|error| RaopError::Crypto(error.to_string()))?;
    public
        .encrypt(&mut rand::thread_rng(), Oaep::new::<Sha1>(), key)
        .map_err(|error| RaopError::Crypto(error.to_string()))
}

fn encrypt_rtp_payload(payload: &mut [u8], key: &[u8; 16], iv: &[u8; 16]) {
    let cipher = Aes128::new_from_slice(key).expect("AES-128 keys are always 16 bytes");
    let mut previous = *iv;
    for block in payload.chunks_exact_mut(16) {
        for (byte, previous_byte) in block.iter_mut().zip(previous) {
            *byte ^= previous_byte;
        }
        cipher.encrypt_block(block.into());
        previous.copy_from_slice(block);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffers_pcm_until_an_alac_chunk_is_ready() {
        let mut bridge = PcmToRaop::new(44_100, 2).unwrap();
        let pcm = vec![0; 352 * 2 * 2];
        let packets = bridge
            .push(PcmAudioFrame {
                sequence: 10,
                timestamp: 100,
                sample_rate: 44_100,
                channels: 2,
                pcm,
            })
            .unwrap();
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].sequence, 10);
        assert_eq!(packets[0].timestamp, 100);
        assert!(!packets[0].payload.is_empty());
    }

    #[test]
    fn rejects_misaligned_pcm() {
        let mut bridge = PcmToRaop::new(48_000, 2).unwrap();
        let error = bridge
            .push(PcmAudioFrame {
                sequence: 0,
                timestamp: 0,
                sample_rate: 48_000,
                channels: 2,
                pcm: vec![0; 3],
            })
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "PCM payload length 3 is not aligned to the sample format"
        );
    }

    #[test]
    fn parses_server_ports() {
        assert_eq!(
            parse_transport(
                "RTP/AVP/UDP;server_port=6000;control_port=6001;timing_port=6002",
                "192.0.2.1".parse().unwrap()
            )
            .unwrap(),
            (
                "192.0.2.1:6000".parse().unwrap(),
                "192.0.2.1:6001".parse().unwrap(),
                "192.0.2.1:6002".parse().unwrap()
            )
        );
    }

    #[test]
    fn builds_rtp_header_shape() {
        let packet = AudioPacket {
            sequence: 3,
            timestamp: 4,
            payload: vec![1, 2],
        };
        assert_eq!(packet.payload.len() + RTP_HEADER_LEN, 14);
    }

    #[test]
    fn parses_digest_challenge() {
        assert_eq!(
            parse_digest_challenge(r#"Digest realm="raop", nonce="abc123""#),
            Some(("raop".into(), "abc123".into()))
        );
    }

    #[test]
    fn computes_digest_md5() {
        assert_eq!(
            md5_hex(b"iTunes:raop:secret"),
            "70b47eb170d318a98a36e7a16a7eae7a"
        );
    }

    #[test]
    fn builds_periodic_sync_packet() {
        let packet = build_sync_packet(48_000, 48_000, false);
        assert_eq!(&packet[..4], &[0x80, 0xd4, 0, 7]);
        assert_eq!(&packet[12..16], &48_000u32.to_be_bytes());
    }
}
