# Mi2Play 技术方案

状态：阶段 2，协议与架构设计完成（2026-10-29）。

## 设计目标

Mi2Play 是一个 Rust workspace。它在局域网中发现 AirPlay/RAOP 播放器，为每个目标发布一个小米妙播虚拟设备，接收 MiPlay 控制面和音频面，再将音频送入对应的 RAOP 播放会话。

首版明确采用“可验证的垂直切片”策略：先完成单设备、模拟 MiPlay 客户端和模拟 RAOP 播放器的端到端链路，再打开真实设备兼容性。任何未在真实硬件上验证的能力都标记为“仅模拟”或“无法确认”。

## Workspace 结构

```text
mi2play/
  crates/
    mi2play-cli/          # mi2play 命令、配置加载、日志和服务入口
    mi2play-core/         # 设备模型、状态机、映射、事件和错误
    mi2play-discovery/    # AirPlay 与 MiPlay mDNS、接口选择
    mi2play-miplay/       # MiPlay 控制帧、RTSP、RTP、会话和能力声明
    mi2play-airplay/      # RAOP v2 client、ALAC/AES、RTP/NTP、音量控制
    mi2play-audio/        # 有界环形缓冲、格式协商、PTS/时钟转换
    mi2play-testkit/      # 协议 fixture、模拟客户端/播放器和集成测试工具
  docs/
    reference-analysis.md
    architecture.md
```

各 crate 通过 typed event/command 接口通信，避免把 socket、编解码器和设备状态绑在一起。运行时使用 Tokio；音频数据不经过无界 channel，所有队列有固定字节/帧上限。

## 运行时数据流

```text
AirPlay mDNS browser ─────┐
                          ▼
                    DeviceRegistry
                          │ 绑定关系
MiPlay mDNS responder ────┤
                          ▼
MiPlay control session ─> VirtualDeviceActor
                          │ commands/events
MiPlay RTP/RTSP media ──> BoundedAudioBuffer
                          │ PTS + clock model
                          ▼
                    RaopOutputActor ──> AirPlay player
```

每个虚拟设备是一个 actor/任务边界，拥有自己的取消令牌、MiPlay session、音频缓冲和 RAOP 输出。注册表只负责发现、绑定和生命周期，不直接写音频。

## 会话生命周期

1. 发现器看到 AirPlay 服务，解析名称、地址、端口、设备 ID、能力 TXT，写入缓存。
2. 配置映射器按 `player_filter`、手动地址和数量创建虚拟设备。
3. MiPlay responder 发布设备身份和能力，使用稳定的本地 device id。
4. 客户端连接控制 socket，完成帧解码、版本/设备信息、认证阶段和能力交换。
5. 客户端发起媒体打开；MiPlay RTSP/RTP 接收器确认格式、时间基准和媒体端点。
6. 音频帧进入有界缓冲；达到启动水位后建立 RAOP session 并按播放时间发送。
7. 控制命令只改变 actor 状态，不阻塞 RTP 读循环。暂停/停止/关闭会 flush 或释放下游资源。
8. 心跳、读写超时和取消统一结束 session；播放器重新上线后可按策略重新绑定，不复用失效 socket。

## 控制冲突优先级

| 来源 | 作用 | 冲突时规则 |
|---|---|---|
| MiPlay 客户端 | 播放、暂停、停止、音量、媒体信息 | 对当前虚拟设备拥有最高控制权 |
| AirPlay 设备反馈 | 连接、能力、错误、可观测播放状态 | 反馈不能覆盖尚未确认的 MiPlay 命令；错误进入设备状态 |
| 本地管理员/CLI | 删除、重绑定、关闭、诊断 | 生命周期操作高于播放操作，安全停止后再重绑 |

AirPlay 设备主动状态查询能力有限，因此不能承诺所有外部控制都能反映给 MiPlay；只发布实际观测到的状态。

## 音频与同步策略

- 首选 MiPlay 公开实现中已观察到的 48 kHz、双声道路径；格式不匹配时返回明确的能力错误，不静默伪造。
- MiPlay RTP/PTS 使用独立时钟模型转换到 RAOP/NTP。模型保存最近有限数量的 RTT/时间样本，剔除异常值并平滑相位/频率。
- 低延迟模式使用较低启动水位，但设置最小安全队列；稳定模式提高水位以吸收抖动。所有模式都限制最大缓存时长。
- 发生轻微漂移时只做小比例速率修正或帧边界调整；严重跳变触发 flush/restart，而不是无限堆积。
- 音频数据面只允许有界 `Bytes`/帧所有权转移，避免多次复制；控制事件使用小消息 channel。

初始默认值会在实现时以配置结构集中定义：低延迟缓冲 120 ms、稳定缓冲 500 ms、最大缓冲 1500 ms。它们必须通过测试和真实设备测量校准，不能在文档中宣称为实测延迟。

## 依赖与许可证原则

- 使用许可证清晰的 Rust crate：Tokio、serde、tracing、socket2、mdns/dns-sd 相关组件需在实现时逐项锁定版本和许可证。
- 不直接依赖 `FusionPlay.MiPlaySdk`、`miplay-bridge` 或无许可证的 `rust-raop-player`。
- 如果从 MIT 的 AirConnect/libraop 借鉴算法，写成独立实现并保留 NOTICE/依赖清单；不得复制 GPL/CC BY-NC-SA 代码。
- 发布前运行许可证检查，并在 `THIRD_PARTY.md` 中列出直接依赖、版本、许可证和用途。

## CLI 与配置草案

命令名统一为 `mi2play`，配置键统一小写：

```text
mi2play --config ./mi2play.toml --log-level info --foreground
mi2play --listen 0.0.0.0 --name "客厅音箱" --player "HomePod"
mi2play --version
mi2play --help
```

建议配置：

```toml
listen = "0.0.0.0"
network_interface = ""
log_level = "info"
foreground = true
device_count = 0
player_filter = []
player_addresses = []
low_latency = true
buffer_ms = 120
max_buffer_ms = 1500
ipv4 = true
ipv6 = false

[[devices]]
name = "客厅"
player = "HomePod"
```

冲突规则：显式命令行覆盖配置文件，单设备映射覆盖自动数量；`player_addresses` 与 `player_filter` 同时存在时取交集，空交集是启动错误；`ipv6=true` 但接口无 IPv6 时只在没有可用 IPv4 时失败；未知键和越界缓冲值必须给出字段名及合法范围。

## 测试策略

### 单元测试

- MiPlay 帧头、命令、RTSP、RTP/MPEG-TS、mDNS TXT 编解码。
- MiPlay/RAOP 状态机的合法和非法转移。
- 映射、配置优先级、设备 ID 稳定性。
- 环形缓冲、时间戳展开、漂移模型、丢包/乱序边界。

### 集成测试

`mi2play-testkit` 提供确定性模拟 MiPlay sender 和 RAOP player，验证单设备链路、暂停/恢复/停止、断线重连、多设备隔离和缓冲上限。测试报告明确标记为模拟，不代表 HomePod 或真实小米客户端兼容。

### 真实设备验证

只有在具备真实小米客户端和 AirPlay 播放器时，才记录发现、认证、音频、控制和重连结果；报告包含型号、OS、网络、音频格式、测量方法和误差。

`.github/workflows/build.yml` 提供 push、pull request 和手动触发的跨平台构建，
产出 Windows x86/x86_64、macOS x86_64/arm64，以及 Linux x86、x86_64、armv7
和 aarch64 的 CLI 压缩包。真实联调建议设置 `RUST_LOG=mi2play=debug`；需要
更细的 RTP/timing/PCM 计数时使用 `trace`，日志不会包含密码或 AES 密钥。

## 分阶段交付

1. **阶段 1/2**：参考分析、许可证边界、架构和协议风险。
2. **阶段 3（当前）**：Rust workspace、配置/CLI、MiPlay 帧状态机和 RAOP 输出层（RTSP/RTP 控制与协议测试）。
3. **阶段 4**：真实 MiPlay 控制/媒体兼容、AirPlay 发现、音量/状态、重连和多设备。
4. **阶段 5**：跨平台构建、服务安装、性能测量、兼容性矩阵和发布文档。

## 当前不能承诺的能力

- 未取得真实客户端/播放器验证前，不承诺 HomePod 认证、所有 HyperOS 版本兼容或完整 RAOP v1/v2 覆盖。
- 当前实现已提供 PCM 数据面适配：`PcmAudioFrame` 接收交错 little-endian 16-bit PCM，经有界分块和 ALAC 编码后送入 RAOP RTP；CLI 用 `--pcm-stdin` 验证该路径。真实 MiPlay 网络会话仍需将解出的 PCM 帧接到该入口。
- `mi2play-miplay` 现在提供 8899 TCP listener/session：按 FusionPlay.MiPlaySdk 的 9-byte framed protocol 处理 SafetyAuth/AES-CBC/HMAC、控制命令和 `OPEN`，再协商 RTSP WFD 的 RTP/MP2T/TCP 媒体流。
- RTP 中的 MPEG-TS/PES/AAC-LC 会解码为 48 kHz 双声道 PCM，并通过事件通道交给 `PcmToRaop`；此前伪造的 `PCM_AUDIO_COMMAND` 路径已删除。
- CLI 会通过 `mdns-sd` 广播带稳定实例名和 TXT 能力字段的
  `_lyra-mdns._udp.local.` 与 `_mi-connect._udp.local.`；`_mi-connect` 的
  SRV 发现端口为 UDP 56666，真实 TCP 8899 控制端口通过 `appsData` 描述。
- RAOP ANNOUNCE 使用固定 AirPlay RSA 公钥封装随机 AES-128 key/IV，RTP ALAC payload 按 AES-CBC 加密，并发送标准 RTP-Info/设备标识头；这不是 AirPlay 2/HAP Pair-Verify 配对。
- AirPlay 1 已支持 RTSP Digest/密码认证、RTP timing 响应、control 丢包请求和 512 包重传缓存，并可浏览 `_raop._tcp` 自动选择播放器。
- 播放期间每秒发送 RTP sync 包，并维护当前音频 RTP timestamp；timing 响应使用接收端请求的 `send_time` 作为 reference。
- RAOP 客户端会在 ANNOUNCE 发送随机 `Apple-Challenge`，并校验接收端返回的 `Apple-Response`；客户端不需要 AirPlay RSA 私钥。真实设备集成测试仍需完成。SafetyAuth 的兼容会话不等同于小米官方设备准入。
- 不承诺视频/镜像、DLNA、Chromecast、Sonos 特有行为。
- 不把参考实现中的私有认证密钥、设备证书或未公开字段当作稳定 API。
