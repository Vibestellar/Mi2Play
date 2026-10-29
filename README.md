# Mi2Play

Mi2Play 是一个以 Rust 编写的跨平台实验性桥接器：将局域网中的 AirPlay/RAOP 播放器映射为小米妙播虚拟设备。

当前仓库已完成参考项目分析和架构设计，正在实现可测试的协议垂直切片。由于小米妙播接收端认证协议存在未公开、版本相关部分，当前版本不会宣称已兼容真实小米客户端或 HomePod；真实设备验证结果会单独记录。

## 开发状态

- 已完成：参考仓库职责、许可证边界、AirConnect 功能映射和总体架构（见 `docs/reference-analysis.md`、`docs/architecture.md`）。
- 实现中：配置解析、设备映射、MiPlay 帧编解码和可验证状态机。
- 已实现：`mi2play-airplay` 的 RAOP RTSP 会话、RSA 封装 AES-128-CBC RTP 音频发送和基本播放控制。
- 已实现：CLI 启动时广播带稳定实例名和 MiPlay TXT 能力字段的
  `_lyra-mdns._udp.local.` 与 `_mi-connect._udp.local.` 服务；`_mi-connect`
  的发现 SRV 使用 UDP 56666，真实 TCP 控制端口 8899 写入 `appsData`；程序
  同时实际监听 UDP 56666 接收手机的发现探测。
- 已实现：AirPlay 1 RTSP Digest/密码认证、RTP timing 响应、丢包控制包重传和 `_raop._tcp` 自动发现。
- 已实现：周期性 AirPlay RTP sync 包和跨序号回绕的重传处理。
- 尚未完成：真实设备集成测试。

`mi2play-miplay` 现在实现了 FusionPlay.MiPlaySdk 对应的控制/媒体路径候选实现：8899 控制帧、SafetyAuth、AES-CBC、HMAC、RTSP WFD 协商、RTP/MP2T/PES、AAC-LC 解码。解码后的 48 kHz 双声道 PCM 会进入 `mi2play-airplay` 的 `PcmToRaop`；真实端口和字段仍需设备抓包确认。

现在 CLI 已支持 PCM 数据面验证：将交错 little-endian 16-bit PCM 通过标准输入送入 RAOP：

```sh
cat audio.pcm | mi2play --raop-address 192.168.1.20:7000 --pcm-stdin
```

PCM 会按 352 samples 分块并编码为 ALAC 后发送。

真实 MiPlay TCP 会话可连接 8899 listener，控制面收到 `OPEN` 后建立 RTSP/RTP 媒体连接，再转发到 RAOP：

```sh
cargo run -p mi2play-cli -- --listen 0.0.0.0:8899 --raop-address 192.168.1.20:7000
```

不指定 `--raop-address` 时，CLI 会浏览 `_raop._tcp.local.` 并选择第一个匹配
`--player-filter` 的 AirPlay 1 接收器；密码设备可通过
`MI2PLAY_RAOP_PASSWORD` 或 `--raop-password` 配置。

真实联调建议使用 `RUST_LOG=mi2play=debug`；`trace` 级别还会记录 PCM/RTP
序号、timestamp、包大小、timing、重传和 RTSP 状态，但不会输出密码、AES
密钥或认证正文。日志目前足以区分发现、连接、认证、SETUP、音频帧、时钟、
重传和断开阶段；仍需要真实设备日志来确认具体厂商字段和时序。

当前实现按参考项目处理 AAC-LC/MPEG-TS；不再使用此前伪造的 `PCM_AUDIO_COMMAND`。

## 构建

需要 Rust 1.85 或更高版本：

```sh
cargo test
cargo run -p mi2play-cli -- --help
```

命令、可执行文件和配置键统一使用小写名称。