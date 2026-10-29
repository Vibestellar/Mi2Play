# Mi2Play

Mi2Play 是一个以 Rust 编写的跨平台实验性桥接器：将局域网中的 AirPlay/RAOP 播放器映射为小米妙播虚拟设备。

当前仓库已完成参考项目分析和架构设计，正在实现可测试的协议垂直切片。由于小米妙播接收端认证协议存在未公开、版本相关部分，当前版本不会宣称已兼容真实小米客户端或 HomePod；真实设备验证结果会单独记录。

## 开发状态

- 已完成：参考仓库职责、许可证边界、AirConnect 功能映射和总体架构（见 `docs/reference-analysis.md`、`docs/architecture.md`）。
- 实现中：配置解析、设备映射、MiPlay 帧编解码和可验证状态机。
- 尚未实现：真实 mDNS responder/browser、完整 MiPlay SafetyAuth、RAOP 输出、真实设备集成测试。

## 构建

需要 Rust 1.85 或更高版本：

```sh
cargo test
cargo run -p mi2play-cli -- --help
```

命令、可执行文件和配置键统一使用小写名称。