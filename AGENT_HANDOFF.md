# Mi2Play Agent Handoff

> 更新时间：2026-11-03  
> 当前分支：`feat/airplay1-miplay-bridge`  
> 代码行为基线：`f0782b0`（本轮 RTP/NTP 同步修正尚未提交）
> 用途：为新 Agent 提供可恢复的技术上下文。阅读本文件后，再查看当前代码和最新真实设备日志；不要把“代码编译通过”误写成“真实设备已验证”。

## 1. 项目目标

Mi2Play 接收手机的真实 MiPlay 会话，并把音频转发到真实 AirPlay/RAOP 音响：

```text
手机 MiPlay
  -> mDNS / TCP 8899 / SafetyAuth
  -> WFD RTSP + interleaved RTP/MP2T/PES/AAC
  -> AAC 解码为 PCM
  -> PCM 重采样 / ALAC
  -> RAOP RTSP + RTP/UDP
  -> AirPlay 音响
```

不要混淆 AirConnect 的方向：AirConnect 是 RAOP 接收端再输出到 UPnP/Chromecast；Mi2Play 是 MiPlay 接收端再作为 RAOP 客户端输出。

## 2. 当前已确认状态

### 已由真实日志确认

- 手机可以发现并选择 Mi2Play。
- MiPlay TCP `8899` 控制会话、设备身份、SafetyAuth、AES-CBC/HMAC 流程可工作。
- WFD RTSP `OPTIONS`、能力查询、`SET_PARAMETER`、`SETUP`、`PLAY`、`TIME_OFFSET` 时序可工作。
- 手机 AAC/ADTS/MPEG-TS/PES 可以解码为持续的 PCM，日志出现 `MiPlay decoded PCM frame pcm_bytes=4096`。
- RAOP `OPTIONS`、`/auth-setup`（当设备广告 `et=4`）、`ANNOUNCE`、`SETUP`、`RECORD`、`GET_PARAMETER volume` 均已获得 `200 OK`。
- PCM 已转换为 ALAC RTP 并由音响接收，真实日志出现连续 RAOP audio packet。
- RAOP timing response、周期 sync、重传基本工作。
- 音量查询可将真实音响例如 `-25.800001 dB` 转成 MiPlay 音量 `14`。
- 手机切回本机时，当前日志已出现 RAOP `TEARDOWN 200`，音响会退出 AirPlay。
- 扫描阶段不再主动建立 RAOP；RAOP 只在加密 MiPlay `OPEN` 后启动。

### 尚需真实设备验证

- 在最新 `cd761ec` 上，停止后同一 MiPlay 控制会话再次 `OPEN` 是否稳定重连。
- 暂停、恢复、停止、重新选择、再次播放的完整闭环。
- 手机多次快速切换时，旧 RAOP 连接结果不会重新占用音响。
- 音量初始查询在不同手机/固件上的 UI 是否完全没有跳变。
- 多设备并行、断网、RAOP 密码设备、RAOP v2/配对设备兼容性。

## 3. 当前关键行为

### MiPlay 扫描与播放门控

`MiPlayServer::accept()` 接受 TCP 连接后会启动 MiPlay 控制会话，但这不应被视为播放开始。当前 `bridge_session()` 不会在 TCP accept、认证、心跳或扫描阶段连接 RAOP。

只有控制面收到加密 `OPEN` 后，`mi2play-miplay` 发送 `SessionEvent::PlaybackRequested`，CLI 才启动 RAOP 连接。收到 PCM 时仍有兜底连接路径，但正常真实播放应先经过 `OPEN`。

### 音量查询

手机可能在扫描/认证阶段发送一个或多个 `GET_VOLUME`。当前实现：

1. 已知音量：立即回复；
2. 未知音量：保存请求序列，不立即回复；
3. 收到正式 `OPEN` 后启动 RAOP；
4. RAOP 建链后执行 `GET_PARAMETER volume`，成功后通过控制通道回传真实音量，并回复所有挂起的 MiPlay `GET_VOLUME`；
5. 从 `OPEN` 开始最多等待 3 秒，RAOP 建链/查询失败或超时才回复 50。

这里的 3 秒是基于已有真实日志中 RAOP 建链约数百毫秒完成、但扫描到正式播放可能相隔十几秒的时序选择。不能把 50 解释为真实音量；它只是明确的 fallback。

### 停止与重连

媒体 RTSP `TEARDOWN`、WFD `wfd_trigger_method: teardown`、媒体 TCP EOF/截断错误会发送 `SessionEvent::Stopped`。CLI 收到后：

- 清空待发送 PCM/待应用音量；
- 将当前 `RaopClient` 取出并发送 `TEARDOWN`；
- 保留 MiPlay 控制会话和桥接循环；
- 后续新的 `OPEN` 可以再次启动 RAOP。

`early eof` 在手机切回本机时通常表示手机主动关闭媒体 TCP；它不是 RAOP teardown 失败的证据。需要关注是否同时出现 RAOP `TEARDOWN 200`。当前实现仍可能在 RTSP 媒体任务和桥接端之间产生重复停止事件，这是可接受但值得后续去重的区域。

## 4. 当前代码入口

- `crates/mi2play-miplay/src/lib.rs`
  - 控制帧、SafetyAuth、加解密、音量挂起/超时、OPEN、WFD RTSP、RTP/TS/PES/AAC、`SessionEvent`。
  - 重要事件：`PlaybackRequested`、`Audio`、`Stopped`、`Disconnected`。
- `crates/mi2play-cli/src/main.rs`
  - `bridge_session()` 管理 MiPlay 控制事件、RAOP 连接、音量查询、PCM 缓冲、停止和重连。
- `crates/mi2play-airplay/src/lib.rs`
  - RAOP RTSP/RTP client、ALAC、AES、Digest、timing、重传、音量、`stop()`/`TEARDOWN`。
- `docs/reference-analysis.md`
  - 参考项目职责、AirConnect 方向差异、音量和生命周期结论。
- `README.md`
  - 项目运行方式和能力说明；其中部分“尚未完成/实验性”文字可能落后于真实联调进度，修改时应避免过度宣称。

## 5. 参考项目和使用边界

- `Vibestellar/AirConnect`：只参考异步状态机、停止生命周期、音量回声抑制和重连思想，不复制其 RAOP 收发方向。
- `Vibestellar/FusionPlay-Android`：MiPlay mDNS、控制命令、SafetyAuth、WFD/媒体参考；版本差异必须以真实日志为准。
- `Vibestellar/miplay-bridge`：MiPlay 帧格式、RTSP 状态机、音量 ACK/通知和媒体管线参考。
- `Vibestellar/libraop`：RAOP 客户端音量范围、SETUP/RECORD、NTP/timing、ALAC/AES 参考。
- `Vibestellar/rust-raop-player`：Rust RAOP 状态、RTP/NTP、重传和同步参考。

参考源码只能证明协议实现的候选形状；真实设备 `200 OK`、实际音频和切换日志才是设备兼容性的证据。

## 6. 提交判断审计说明

上一版交接文档的提交判断**不够可靠**：它主要把 `git log`、当前代码和压缩摘要拼在一起，没有逐条核对当时的真实设备日志，因此把“后续阶段已经能继续”误写成“该提交已经解决”。本节及下表按会话 checkpoint 和用户日志重新收紧结论。

特别需要记住：

- `cargo check/test` 只能证明代码和模拟测试，不证明手机或音响接受了该协议。
- 后续提交成功，不能证明前一个提交解决了问题；很多提交只是把失败从一个协议阶段推进到下一个阶段。
- `365d22c` 的 RSA v1.5 修改与 `libraop`/`rust-raop-player` 的 OAEP 参考相冲突，不能标为成功修复。
- `e9cd3f8`、`8c6f6e6` 之后真实日志仍有 `SETUP 500`；真正出现 `SETUP 200` 是后续 socket/timing/调度修改之后，不能把前两个提交单独算作解决。
- `b2d0762` 解决了控制面被媒体接收阻塞的一部分问题，但没有解决媒体结束到 RAOP teardown 的传播；当时用户日志明确显示 sync 持续到 Ctrl+C。
- `c459350` 增加了停止传播和未知音量模型，但也引入/暴露了“MiPlay TCP 会话一建立就提前 RAOP 建链”的错误行为；用户扫描日志明确证明该版本不可接受。
- `9f8a7b2` 只解决扫描阶段不建立 RAOP，不能代表音量闭环或重连闭环已解决。
- `0d88f2d` 的超时起点代码修复尚未由真实设备 UI 证明；后续用户日志仍需要检查是否使用了包含该提交的二进制。
- `cd761ec` 只是针对“停止后桥接循环退出、第二次 OPEN 无消费者”的代码修复；截至文档生成时没有最新真实日志证明重复切换已经稳定。

因此，下表的“判断性质”是**截至对应提交的证据边界**，不是对整个后续历史的回溯性背书。

## 7. 每次提交记录

状态标签：

- **已解决/真实验证**：提交目标已由真实日志或明确行为证实。
- **部分解决**：代码或单元测试通过，但仍有真实设备边界。
- **基础/代码验证**：提供实现基础，不能单独证明真实设备兼容。
- **假设/待验证**：提交依赖协议推断，后续需要抓包或设备日志确认。

| 提交 | 目的与主要变化 | 结果与证据 | 判断性质 |
|---|---|---|---|
| `0da82c4` | 合并 MiPlay foundation 分支。 | 只改变历史拓扑，无独立协议行为。 | 基础 |
| `392d7d1` | 初始 MiPlay 到 AirPlay 1 bridge、协议垂直切片和 crate 结构。 | 建立总体骨架；当时未证明真实端到端。 | 基础/未验证 |
| `f206fe3` | 增加跨平台构建和诊断 CI。 | 改善构建可见性，不解决协议问题。 | 代码验证 |
| `0f3d7b9` | 使用完整 MiPlay mDNS service type。 | 修正发现记录形状；后续手机发现日志证明方向正确。 | 部分解决/真实验证 |
| `aa5800e` | 修正 MiPlay discovery metadata/TXT。 | 为手机识别提供必要元数据；随后发现链路继续推进。 | 部分解决/真实验证 |
| `3568264` | 绑定 MiPlay discovery UDP 端口。 | 使发现探测能到达程序；不是音频或认证实现。 | 已解决发现子问题 |
| `cc984e3` | 把 RAOP 连接延迟到收到 MiPlay 音频。 | 当时避免扫描即连，但后来为音量查询和播放门控重新调整。 | 已解决当时问题/后续替代 |
| `21cbf50` | 发布可识别的 MiPlay mDNS identity。 | 支持手机显示/选择设备；真实发现日志确认。 | 已解决/真实验证 |
| `3445a23` | 对齐 FusionPlay 的 MiPlay mDNS records。 | 改善不同客户端识别兼容性；真实手机可继续连接。 | 部分解决/真实验证 |
| `bbe4967` | 完成 MiPlay capability handshake。 | 手机能推进到后续控制命令；真实日志出现能力阶段。 | 已解决/真实验证 |
| `358d8ac` | 调整 capability notification 顺序。 | 修正手机握手时序；后续 SafetyAuth/加密帧证明有效。 | 已解决/真实验证 |
| `95ace6c` | 暴露可配置 MiPlay diagnostics。 | 改善排查能力，不改变核心协议。 | 代码验证 |
| `e13937c` | 先发送 MiPlay device identity。 | 对齐手机预期的初始帧；真实会话日志出现 identity announced。 | 已解决/真实验证 |
| `e621c70` | 支持 wrapped MiPlay SafetyAuth handshake。 | 手机可继续完成认证；真实日志出现 `outer=20` 和后续加密帧。 | 已解决/真实验证 |
| `6e0cc44` | 分离 MiPlay 控制读写 cipher 方向。 | 修正加密方向，避免认证后解密错位。 | 已解决/真实验证 |
| `c033b19` | 让 MiPlay AES IV 从 ciphertext 推进。 | 修正跨帧 CBC 状态；后续连续控制帧和心跳证明有效。 | 已解决/真实验证 |
| `461ccf2` | 使用 FusionPlay 兼容的 authentication key 表示。 | 认证 key 形状与参考实现一致；真实 SafetyAuth 成功。 | 已解决/真实验证 |
| `88a9635` | 编码 MiPlay device info payload。 | 手机可继续设备信息阶段；真实日志推进到媒体控制。 | 已解决/真实验证 |
| `c1675b2` | 对齐 MiPlay WFD RTSP setup order。 | RTSP 能进入 `SETUP/PLAY`；真实日志确认。 | 已解决/真实验证 |
| `d37b8d1` | 保留 MiPlay RTSP presentation URL。 | 修正手机提供的 stream URL 使用；真实日志出现正确 presentation URL。 | 已解决/真实验证 |
| `791bf6c` | 拆分单个 PES 中的多个 ADTS frames。 | PCM 连续输出恢复；真实日志持续出现 4096-byte PCM。 | 已解决/真实验证 |
| `7bc770f` | 强制 decoder 使用 ADTS framing。 | 避免容器自动探测误判；后续 AAC 播放继续工作。 | 部分解决/真实验证 |
| `2ea7c77` | 支持单个 ADTS frame 的多个 raw-data-block。 | 覆盖更多 AAC 帧形状；测试通过，真实设备未单独隔离验证。 | 部分解决/代码+真实链路 |
| `0b6c8ec` | 支持 scoped IPv6 RAOP endpoints。 | 扩展 endpoint 解析；无本轮真实 IPv6 证据。 | 代码验证 |
| `b865073` | 发送兼容 RAOP ANNOUNCE headers。 | 为真实音响接受 ANNOUNCE 铺路。 | 部分解决/真实验证 |
| `27f172a` | 增加详细 RAOP RTSP tracing。 | 直接暴露请求/响应时序，帮助定位后续 403/500。 | 已解决诊断问题 |
| `68735e8` | 将 MiPlay PCM 重采样到 RAOP 44.1 kHz。 | 音响可接收标准 RAOP 采样率；真实 RTP 音频日志确认。 | 已解决/真实验证 |
| `365d22c` | 使用 RAOP RSA v1.5 key encryption。 | 后续核对 `libraop` 和 `rust-raop-player` 均使用 OAEP；该改动没有被真实设备单独证明，后来被 OAEP 方向替代。 | 未解决/错误方向猜测 |
| `188b2de` | 对齐 RAOP SDP 和远端地址。 | 修正 ANNOUNCE 内容；为 200 OK 做准备。 | 已解决/真实验证 |
| `54ef25d` | 对齐 RAOP URL、sid、User-Agent 和 session 时序。 | ANNOUNCE 从 403 走向成功；真实日志确认。 | 已解决/真实验证 |
| `1eb51c4` | 默认使用 clear RAOP audio negotiation。 | 避免错误地协商音频加密；真实音频 RTP 成功。 | 已解决/真实验证 |
| `b84de25` | 根据 mDNS `am` 选择 RAOP User-Agent。 | 提升不同 AirPlay 设备兼容性；真实设备继续接受。 | 部分解决/真实验证 |
| `7fc095c` | 保持 RAOP request header ordering。 | 修正严格设备解析差异；无单独隔离证据，但后续 SETUP 成功。 | 部分解决/真实验证 |
| `4f7e653` | 当 mDNS `et=4` 时执行 X25519 `/auth-setup`。 | 真实音响 `/auth-setup` 返回 200/1076 bytes。 | 已解决/真实验证 |
| `e9cd3f8` | 对齐 RAOP SETUP 和 playback state。 | 真实设备仍返回 `SETUP 500`；只是协议形状尝试，没有单独解决。 | 未解决/猜测性修复 |
| `8c6f6e6` | 完善 RAOP SETUP 和 MiPlay 音量控制。 | 音量协议代码增加，但真实设备仍有 SETUP 失败，手机音量仍出现少数档位/初始中间值；不能称为解决。 | 未解决/部分代码验证 |
| `3c5a45b` | 绑定 RAOP sockets 到本机地址并改善 trace。 | 与后续 timing/调度修改共同推进到 `SETUP 200`；无法仅凭后续日志证明本提交单独解决。 | 部分解决/因果未隔离 |
| `cdd4fd9` | 修正 timing 时序和 MiPlay bridge 调度。 | 之后真实日志出现 `SETUP 200`、`RECORD 200` 和 RTP；这一阶段解决了建链/控制阻塞问题，但后续 teardown、初始音量和重连仍失败。 | 部分解决/真实验证 |
| `b2d0762` | 保持 MiPlay RTSP 控制面活跃，增加 RAOP 音量查询同步。 | 音量查询真实成功；但用户日志中媒体结束后 RAOP sync 仍持续到 Ctrl+C，teardown 缺口未解决。 | 部分解决/明确未完成 |
| `9597f61` | 修正文档中的仓库职责和许可证描述。 | 只影响 `docs/reference-analysis.md`，不改变运行行为。 | 文档验证 |
| `c459350` | 用未知音量 + 挂起 GET_VOLUME、媒体停止事件触发 RAOP teardown。 | 代码引入了停止事件和音量挂起，但真实日志随后证明扫描阶段仍提前建立 RAOP；该版本行为不可接受。 | 部分解决且引入新问题/真实反证 |
| `9f8a7b2` | 将 RAOP 连接门控到加密 MiPlay `OPEN`。 | 用户日志确认扫描阶段不再出现 RAOP，正式播放阶段出现；只解决扫描误连接，不解决音量 fallback 或重连。 | 已解决单一问题/真实验证 |
| `0d88f2d` | 将 3 秒音量 fallback 从 GET_VOLUME 时刻延后到 `OPEN` 时刻。 | 代码意图正确，但真实 UI 尚未证明；此前用户日志明确显示 50 fallback 早于 OPEN，必须确认运行二进制包含本提交。 | 代码修复/真实结果未确认 |
| `cd761ec` | 停止播放只 teardown 当前 RAOP，保留 MiPlay bridge/control loop。 | 针对日志中“第一次停止后第二次 OPEN 无消费者”的根因修复；仅编译测试通过，重复真实切换尚未证明。 | 未完成真实验证 |

## 8. 已知未解决或曾失败的修改

这些不是“历史细节”，而是新 Agent 不得重复宣称已解决的事项：

| 问题 | 相关提交/阶段 | 真实证据 | 当前结论 |
|---|---|---|---|
| RAOP `SETUP 500` | `e9cd3f8`、`8c6f6e6` 及更早尝试 | 音响返回 `500 Internal Server Error`，没有 `Session`/远端端口，也没有 `RECORD` | 后续 `cdd4fd9` 阶段才出现 200；因果不能归给单个早期提交 |
| 音量只有少数档位/初始跳到 50 | 早期音量实现、`8c6f6e6`、`b2d0762`、`c459350` | 用户多次报告手机显示中间值后跳真实值；`GET_VOLUME` 曾返回硬编码 50 | 只有协议格式和挂起机制部分完成，真实 UI 闭环仍需验证 |
| 扫描就让音响显示 AirPlay | `c459350` | 用户提供日志：认证后、没有 `OPEN`/PCM 时已经出现 RAOP `OPTIONS→RECORD` | `9f8a7b2` 已修复该单一问题 |
| 媒体结束不 teardown RAOP | `b2d0762` 之前/当时 | `early eof` 后 RAOP sync 持续到 Ctrl+C 才 `TEARDOWN` | 后续 `c459350` 增加事件，但生命周期仍需真实验证 |
| 第一次切回后再次切换失败 | `c459350`、`cd761ec` | 第一次停止后桥接任务退出；下一次 `OPEN` 没有 RAOP 日志 | `cd761ec` 只完成代码修复，尚无重复切换实测证据 |
| `early eof`/`broken pipe` | 多个媒体停止阶段 | 手机关闭媒体 TCP 后出现 `early eof`；某次下一轮播放出现 `broken pipe/truncated` | `early eof` 本身可能是正常媒体收尾；`broken pipe` 暴露旧桥接生命周期问题，不能统称“已解决” |
| RSA v1.5 方向 | `365d22c` | 两个指定参考项目使用 OAEP；没有真实成功证据支持 v1.5 | 应视为被后续参考核对否定的尝试 |

## 9. 重要真实日志证据

### 2026-11-03 最新无声验证

本轮运行二进制对应仓库 `HEAD=f0782b0`。用户日志证明：

- `f0782b0` 所含代码可以完成 MiPlay 认证、WFD `SETUP/PLAY`、AAC 解码、RAOP `ANNOUNCE/SETUP/RECORD`、音量查询和持续 RTP 发送；
- 但真实音响仍然无声音，因此不能把 `f0782b0` 或其父提交 `e382004` 记为解决无声问题；
- 日志中的第一条音频 RTP 使用 `timestamp=4294967295`，后续按 352 samples 增长；这暴露了首始时间戳和同步时间基准仍需修正；
- 日志中的 `Bridging MiPlay PCM to RAOP sequence=0 timestamp=0` 是 MiPlay PCM 帧的局部序号，不等同于 RAOP 实际发送序号；RAOP 内部日志已显示序号递增，不能据此断定发送参数恒定；
- `e382004` 将 sync 包 NTP 字段改成发送瞬间墙上时钟，但参考 `libraop` 要求 NTP 与 RTP timestamp 使用同一映射；因此该提交只能标为未验证的同步尝试，不能标为已解决。

随后代码将 RAOP RTP 首始 timestamp 改为随机起点，并保存固定 RTP-to-NTP 映射，使首个 sync、周期 sync 和音频 RTP 使用一致时间基准。该修改目前只有代码和单元测试证据，必须在真实音响上重新验证是否出声。

### 扫描不应建立 RAOP

旧行为的证据是：认证后尚未 `OPEN`，却直接出现 RAOP `OPTIONS`。这由 `c459350` 的“会话启动即连接”引入，已由 `9f8a7b2` 修复。

修复后的正确时序应类似：

```text
MiPlay authentication / GET_VOLUME
... no RAOP ...
MiPlay command=64
MiPlay command=0 (OPEN)
MiPlay playback requested; connecting to RAOP receiver
RAOP OPTIONS -> ANNOUNCE -> SETUP -> RECORD -> GET_PARAMETER volume
```

### 音量

曾见到：

```text
GET_VOLUME at 03:55:51
fallback volume=50 at 03:55:54
OPEN at 03:55:... later
```

这证明 3 秒计时点错误；`0d88f2d` 已改为从 `OPEN` 开始计时。最新真实验证应重点观察：扫描阶段不能出现 fallback，正式 `OPEN` 后应先出现 RAOP volume 查询，再回复挂起的 MiPlay GET_VOLUME。

### 停止和第二次播放

旧日志中的典型问题：

```text
SET_PARAMETER cseq=6
RAOP TEARDOWN 200
bridge_session exits
next OPEN arrives
no RAOP connection
broken pipe / truncated MiPlay frame
```

`cd761ec` 的目标是让停止只清理 RAOP，而不退出控制桥接循环。若仍失败，应记录：

- 每次 `PlaybackRequested` 是否出现；
- 每次 RAOP connect task 的开始、成功、失败；
- 停止时 `playback_active` 的状态；
- 第二次 `OPEN` 是否在旧 connect task 尚未完成时到达；
- 是否出现第二个 RAOP `OPTIONS`。

### 最新联调提交核验：`e382004`

用户提供的日志文件 `d60d3f5.log` 对应的是 `e382004` 之前的代码状态。该日志直接证明：

- MiPlay AAC 解码和 PCM 输出持续成功；
- RAOP `ANNOUNCE`、`SETUP`、`RECORD`、`GET_PARAMETER volume` 均返回 `200`；
- RAOP RTP 音频包持续发送；
- 但首个 RTP 音频包时间戳为 `4294967295`，随后从 `351` 开始；
- 原同步包没有按 `libraop` 参考实现发送 `timestamp - 11025` 的 latency 时间戳，且 NTP 字段构造不正确；
- 用户实际结果仍是“无声音”。

因此 `e382004` 修正了一个有日志和参考代码依据的 RAOP 同步错误，但**截至本文更新时没有该提交之后的真实设备日志**，不能称为已解决。下一次联调必须明确确认：

```text
e382004 binary/version
RAOP sync packet
RTP first timestamp
音响是否实际出声
```

### 联调提交核验规则

以后每轮真实联调都必须：

1. 以用户反馈日志和现象为准，检查上一次 commit 是否真的解决目标问题；
2. 区分“日志直接证明”“代码/单测证明”“参考实现支持的假设”和“尚未验证”；
3. 把该核验结果追加到本文件，不能只根据 commit message 或后续阶段成功反推；
4. 若没有新实机日志，只能记录为“待验证”，不得写成“已解决”。

## 10. 推荐下一步

1. 在 `cd761ec` 上做真实测试：扫描但不播放、播放、切回本机、再次切回 Mi2Play、暂停/恢复、快速重复切换。
2. 反馈日志默认使用 `RUST_LOG=mi2play=debug,mi2play_miplay=debug,mi2play_airplay=debug`；不要长时间使用 `trace`。`trace` 仅在需要逐包核对 RTP/RTSP 时短时间开启，并限制在一次播放会话。
3. 若第二次切换仍失败，先加 session id / playback generation 日志，不要猜命令号。
4. 给 `MiPlaySession` 和 `bridge_session` 增加播放 generation 或 cancellation token，防止旧 RTSP/RTP task 的迟到 `Stopped` 关闭新播放。
5. 对重复 `Stopped` 做 generation 过滤；当前媒体 EOF、RTSP teardown 和控制事件可能对同一播放产生多个停止通知。
6. 增加单元测试：
   - 扫描阶段 `GET_VOLUME` 不启动 RAOP；
   - `OPEN` 才发 `PlaybackRequested`；
   - `OPEN` 后 3 秒 fallback；
   - 停止后同一桥接循环可处理第二次 `OPEN`；
   - 旧媒体 task 的停止事件不能关闭新 RAOP session。
7. 真实验证通过后再更新 README 的“真实设备集成测试”描述；不要只凭单元测试修改兼容性声明。

## 11. 验证命令

```sh
cargo fmt --all -- --check
cargo check -p mi2play-miplay -p mi2play-airplay -p mi2play-cli
cargo test -p mi2play-miplay -p mi2play-airplay -p mi2play-cli
git diff --check
git status --short --branch
```

当前提交历史包含 Co-authored-by trailer。后续提交继续使用：

```text
Co-authored-by: Copilot <223556219+Copilot@users.noreply.github.com>
```

## 12. 新 Agent 启动指令

新会话开始时使用：

```text
请先阅读仓库根目录 AGENT_HANDOFF.md、docs/reference-analysis.md，
再检查当前 HEAD 和工作区。以真实日志为证据继续 MiPlay -> RAOP 工作，
不要重复已经完成的协议调查，也不要把猜测写成已验证事实。
```

## 13. 2026-11-03 最新无声反馈后的日志与时间线调整

用户反馈：最新完整会话仍然无声音，并指出长时间 `trace` 日志浪费反馈 token。

- 真实日志仍只证明 MiPlay PCM 解码、RAOP 建链和 RTP 发送成功，不能证明音响实际渲染音频。
- 将 MiPlay RTSP 每条消息日志从 `debug` 降为 `trace`；逐 RTP 包、timing 和完整 RTSP 正文继续只在 `trace` 下输出。
- 后续常规反馈使用 `RUST_LOG=mi2play=debug,mi2play_miplay=debug,mi2play_airplay=debug`；一次联调只在需要逐包定位时短时启用 `trace`。
- 修正 `PcmToRaop` 不再用 MiPlay 音频帧中的零值 sequence/timestamp 覆盖 RAOP 自有 RTP 时间线。该问题有代码证据，但尚未有新的真实设备日志证明已经恢复出声。
- 最新日志显示 MiPlay 在同一段播放中反复发送 `command=6`，旧实现会为每次命令重复发送 `RECORD Range: npt=0-`；已改为仅在暂停后第一次恢复时发送 `RECORD`，并增加每 100 个音频 RTP 包一次的 debug 汇总，避免逐包反馈。
- 2026-11-03 13:45 实机日志包含两轮播放/暂停：首轮从约 13:45:07 持续发送 RTP，13:45:36 收到两次暂停并完成两次 `FLUSH 200`；但发送统计仍从约 3600 增长到 5300，说明暂停后仍在发送音频。第二轮恢复后又继续发送到 10000 包，音响仍无声。已让 `send_pcm` 在 RAOP 暂停状态直接丢弃 MiPlay 音频帧，避免 `FLUSH` 后继续发送 RTP；该修复尚待下一次实机验证。
