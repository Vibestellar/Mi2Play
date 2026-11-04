# Mi2Play 参考仓库分析

## 参考仓库职责

| 仓库 | 主要职责 | 可复用内容 |
|---|---|---|
| `Vibestellar/AirConnect` | AirPlay 接收端、UPnP/Chromecast 发现、虚拟 AirPlay 设备、音频代理 | 生命周期、设备映射、配置/日志/服务运行经验 |
| `Vibestellar/FusionPlay-Android` | Android 应用及独立 `src/FusionPlay.MiPlaySdk` | MiPlay mDNS、控制帧、SafetyAuth、RTSP/RTP、时钟同步的观测实现 |
| `Vibestellar/miplay-bridge` | Go MiPlay 接收端和本地声卡输出 | 帧头/命令、RTSP 状态机、MiPlay mDNS、媒体 RTP/MPEG-TS |
| `Vibestellar/libraop` | C RAOP v2 播放器/库，含 ALAC、AES、配对和同步 | RAOP client API、RTP/NTP、ALAC、音量和进度模型 |
| `Vibestellar/rust-raop-player` | Rust RAOP v2 播放器和同步控制器 | Rust 中的 RTP、NTP、ALAC、AES、重传、同步抽象 |
## 关键实现位置

### AirConnect

- `aircast/src/`：AirPlay 接收端、虚拟设备和 Chromecast 输出。
- `airupnp/src/`：UPnP/Sonos 发现、媒体控制和 HTTP 音频代理。
- `airupnp/src/airupnp.c` 的 `HandleRAOP` 将 `RAOP_FLUSH`、`RAOP_STOP`、`RAOP_PLAY` 和 `RAOP_VOLUME` 转换为下游播放器状态；播放时不主动覆盖音量，等待 AirPlay 控制器发送音量事件。
- `airupnp/src/airupnp.c` 用 `VolumeStampRx/VolumeStampTx` 做约 1 秒的回声抑制，并在下游音量变化后通过 `raopsr_notify(..., RAOP_VOLUME, ...)` 回传归一化的 `0..1` 音量。Mi2Play 可复用这个“单一状态 + 时间戳抑制回环”模型，但不能直接把 UPnP 的整数音量协议当成 MiPlay 的 `uint32_be` 百分比。
- `aircast/src/castcore.c` 在媒体会话建立或重连后重新发送 `SET_VOLUME`，说明输出会话的创建与音量状态恢复应分离；Mi2Play 的 MiPlay `SET_VOLUME` 也必须先更新本地状态，不能依赖 RAOP `SETUP` 成功。
- `airupnp/src/airupnp.c` 对 RAOP 的异步回调不阻塞主控制线程；Mi2Play 因此不能在消费 MiPlay 事件的循环中同步等待 RAOP 建链。
- README 的命令行/配置说明明确了：mDNS UDP 5353、每设备持久 RTSP 端口、播放期间 HTTP/RTP 端口、周期重扫、配置文件覆盖、前台/后台模式和端口范围。
- 对 Mi2Play 有用的架构模式是“发现器 -> 设备注册表 -> 每设备会话 -> 有界音频管线”，而非其 UPnP/DLNA 具体实现。

本轮按上述证据调整了 Mi2Play：RAOP timing worker 在 `ANNOUNCE` 后、`SETUP` 前启动，并使用未连接 UDP socket 对收到的 timing 请求直接回复；MiPlay 音频首帧触发的 RAOP 连接改为后台任务，控制事件（尤其音量）继续消费，连接完成后再发送有限的待发送音频和最新音量。该改动解决的是已确认的时序/阻塞问题，不等同于真实设备已经接受 `SETUP`；真实 `SETUP 200`、`RECORD 200` 和实际出声仍需设备日志验证。

### FusionPlay MiPlay SDK

- `src/FusionPlay.MiPlaySdk/src/discovery.rs`：`_lyra-mdns._udp.local.`、`_mi-connect._udp.local.`、IPv4/IPv6 mDNS responder/browser、设备身份和 TXT 数据。
- `src/FusionPlay.MiPlaySdk/src/protocol.rs`：控制帧、版本/设备信息、SafetyAuth、音量、媒体信息、状态、心跳和通知。
- `src/FusionPlay.MiPlaySdk/src/media.rs`：48 kHz 双声道输入假设、AAC/ADTS、RTP/MPEG-TS、RTSP、PTS、时钟回归、有限缓冲和漂移修正。
- `src/FusionPlay.MiPlaySdk/src/lyra.rs`：Lyra/KCP 传输和加密控制面；该部分必须视为“兼容性实验”，不能宣称覆盖所有小米客户端或设备代际。
- SDK README 明确指出公开资料不包含通用接收端设备证书/硬件信任根，因此“发现”不等于“可认证会话”。

### miplay-bridge

- `internal/miplay/protocol.go`：9 字节帧头、最大 payload、防御式解码和命令枚举。
- `internal/miplay/mdns.go`：设备身份、TXT、mDNS responder、地址发现和 goodbye。
- `internal/miplay/control.go`：Legacy challenge、SafetyAuth、设备信息、状态、媒体信息、音量、心跳及暂停/恢复/关闭。
- `internal/miplay/rtsp.go`：MiPlay RTSP 阶段状态机、计时器端点和能力响应。
- `internal/miplay/media.go`：媒体帧解码、RTP/MPEG-TS、48 kHz PCM 管线和有界诊断统计。
- 该项目的音频输出是本地声卡/FFmpeg，不包含 AirPlay 输出，不能直接满足 Mi2Play 的目标。

### libraop

- `src/raop_client.c/.h`：连接、修复、flush、keepalive、进度、音量、发送音频 chunk、播放/暂停/停止。
- `src/raop_streamer.c/.h`：服务端流会话、RTP 帧、flush、metadata。
- `src/alac.c`、`src/aes*.c`、`src/pairing.cpp`：ALAC、AES 和配对支持。
- README 明确支持 Windows、macOS、Linux x86/ARM，且使用 NTP 进行多播放器同步。

### rust-raop-player

- `src/raop_client.rs`：RAOP 客户端状态机、setup 参数、队列和状态。
- `src/rtp.rs`：音频、同步、重传和丢包报告帧。
- `src/codec.rs`、`src/crypto.rs`：编码/加密抽象。
- `src/sync_controller.rs`、`src/timing_controller.rs`、`src/ntp.rs`：NTP、时钟和播放同步。
- `Cargo.toml` 显示 Rust 2024、AES/ALAC、RSA、Ed25519、X25519、Tokio 等依赖。

## 许可证与依赖结论
必要时可移植成为目标代码

## AirConnect 到 Mi2Play 功能映射

| AirConnect 能力 | Mi2Play 状态 | 说明 |
|---|---|---|
| mDNS/Bonjour 发现真实播放器 | 必须实现 | 发现 AirPlay `_raop._tcp`/`_airplay._tcp`，维护上线/离线缓存 |
| 自动创建任意数量虚拟设备 | 必须实现 | 每个 MiPlay 虚拟设备绑定一个 AirPlay 播放器 |
| 设备筛选、手动地址、接口选择 | 必须实现 | CLI 与配置文件统一为小写键名 |
| 音频代理和有界缓冲 | 必须实现 | MiPlay 输入到 RAOP 输出，控制面与数据面分离 |
| 播放/暂停/停止/继续 | 必须实现 | 受 MiPlay 客户端控制，并映射到 RAOP session |
| 音量和静音 | 应实现 | AirPlay 设备能力不同，失败时必须报告状态 |
| metadata、进度、曲目切换 | 应实现 | 先保存/转发可验证字段，不能伪造播放器状态 |
| 多设备并行 | 应实现 | 每设备独立 session、缓冲和故障域 |
| NTP/时钟漂移修正 | 应实现 | 首版使用单设备同步；多设备同步后置 |
| 编解码重编码 | 可选实现 | 优先透明转发或单次 AAC/PCM 解码，避免无必要转换 |
| UPnP/DLNA 设备发现 | 暂不支持 | 非 Mi2Play 核心需求 |
| Chromecast/Sonos 特有参数 | 暂不支持 | 不复制 AirConnect 的目标设备语义 |
| 视频、投屏镜像 | 暂不支持 | 项目范围明确为音频桥接 |
| daemonize/系统服务包装 | 可选实现 | 首版先提供前台运行和可用于 systemd/launchd/NSSM 的参数 |

## 主要技术风险

1. **MiPlay 认证风险**：公开实现可能只能完成发现或 Legacy 会话，SafetyAuth/设备身份可能因客户端版本而变化。
2. **AirPlay 版本差异**：HomePod 等设备可能要求特定 RAOP v2、配对或加密能力；必须保留能力探测和明确错误。
3. **时钟域转换**：MiPlay 的 90 kHz RTP/PTS 与 RAOP/NTP 时间基准不同，需要单调时钟、有限队列和漂移修正。
4. **多网卡与组播**：mDNS 在容器、VPN、Windows 防火墙和 IPv6 环境中容易失败，必须支持接口绑定和诊断。
