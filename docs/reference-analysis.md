# Mi2Play 参考仓库分析

状态：阶段 1，代码库分析完成（2026-10-29）。

## 参考仓库职责

| 仓库 | 主要职责 | 可复用内容 | Mi2Play 处理方式 |
|---|---|---|---|
| `Vibestellar/AirConnect` | AirPlay 接收端、UPnP/Chromecast 发现、虚拟 AirPlay 设备、音频代理 | 生命周期、设备映射、配置/日志/服务运行经验 | MIT 允许复用思想；不复制 C 实现，重新用 Rust 建模 |
| `Vibestellar/FusionPlay-Android` | Android 应用及独立 `src/FusionPlay.MiPlaySdk` | MiPlay mDNS、控制帧、SafetyAuth、RTSP/RTP、时钟同步的观测实现 | AGPL-3.0；不直接依赖或复制，协议兼容性作为独立实现目标 |
| `Vibestellar/miplay-bridge` | Go MiPlay 接收端和本地声卡输出 | 帧头/命令、RTSP 状态机、MiPlay mDNS、媒体 RTP/MPEG-TS | CC BY-NC-SA 4.0（含特别商业限制）；仅作协议和测试行为参考 |
| `Vibestellar/libraop` | C RAOP v2 播放器/库，含 ALAC、AES、配对和同步 | RAOP client API、RTP/NTP、ALAC、音量和进度模型 | MIT；优先参考接口与时序，避免绑定 C ABI；必要时后续评估独立 Rust 依赖 |
| `Vibestellar/rust-raop-player` | Rust RAOP v2 播放器和同步控制器 | Rust 中的 RTP、NTP、ALAC、AES、重传、同步抽象 | 仓库当前没有可确认的 LICENSE/COPYING；不作为依赖或复制源，需先取得明确授权 |

## 关键实现位置

### AirConnect

- `aircast/src/`：AirPlay 接收端、虚拟设备和 Chromecast 输出。
- `airupnp/src/`：UPnP/Sonos 发现、媒体控制和 HTTP 音频代理。
- README 的命令行/配置说明明确了：mDNS UDP 5353、每设备持久 RTSP 端口、播放期间 HTTP/RTP 端口、周期重扫、配置文件覆盖、前台/后台模式和端口范围。
- 对 Mi2Play 有用的架构模式是“发现器 -> 设备注册表 -> 每设备会话 -> 有界音频管线”，而非其 UPnP/DLNA 具体实现。

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
- `Cargo.toml` 显示 Rust 2024、AES/ALAC、RSA、Ed25519、X25519、Tokio 等依赖，但没有许可证声明。

## 许可证与依赖结论

1. Mi2Play 首版不直接 vendoring、复制或链接 `miplay-bridge`、`FusionPlay.MiPlaySdk` 或 `rust-raop-player`。
2. AirConnect 和 libraop 的 MIT 许可证允许参考和复用，但其第三方依赖仍需逐项核对；实现将优先使用 Rust 生态中许可证清晰的 crate。
3. 如果未来需要直接使用 AGPL 代码，Mi2Play 必须明确采用兼容许可证并发布完整对应源代码；本设计不把该约束隐藏在可选依赖中。
4. MiPlay 的认证/准入部分未公开，所有支持等级必须区分“协议模拟测试”“发现成功”“真实客户端会话成功”。

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
5. **商业许可证**：参考仓库中存在非商业和 AGPL 代码，不能通过代码相似性规避许可证义务。
