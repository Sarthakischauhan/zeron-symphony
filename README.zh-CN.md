# Zeron Symphony

基于 Rust/GPUI 的原生桌面工作区，使用 [Symphony](https://github.com/Sarthakischauhan/symphony) 作为唯一生产运行时。Zeron 提供聊天界面、终端、浏览器、差异查看和可选设备同步；Symphony 管理模型、工具、技能、插件、审批和会话检查点。

## 从源码运行

需要 Python 3.11+、uv、稳定版 Rust 和平台对应的 GPUI 编译依赖。

```bash
git clone https://github.com/Sarthakischauhan/zeron-symphony.git
git clone https://github.com/Sarthakischauhan/symphony.git
cd symphony
uv sync --all-packages
cd ../zeron-symphony
export SYMPHONY_EXECUTABLE="$(cd ../symphony && pwd)/.venv/bin/symphony"
cargo build --release --locked -p zeron
./target/release/zeron
```

Symphony 必须支持第 2 版 stdio 协议。模型提供商可通过启动环境或设置中的 Symphony 账户配置。远程引擎所在设备也必须安装 Symphony。

详见 [集成指南](docs/symphony.md)、[架构](ARCHITECTURE.md)及[文档索引](docs/README.md)。

## 许可

[MIT](LICENSE)。
