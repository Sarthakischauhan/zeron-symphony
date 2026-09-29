# Zeron Symphony

Zeron 的桌面界面，使用 [Symphony 编码 Agent](https://github.com/Sarthakischauhan/symphony) 作为运行引擎。Zeron 提供工作区、聊天和 Agent 界面；Symphony 负责运行模型和工具。

*[English](README.md) | 简体中文*

![Zeron 桌面截图，展示正在进行的编码会话](apps/landing/public/assets/app-screenshot.jpg)

## 从源码安装

需要 Python 3.11+、[uv](https://docs.astral.sh/uv/) 和稳定版 Rust 工具链。启动 Zeron 前，请先在 Symphony 中配置至少一个模型提供商。

将两个功能分支克隆到同一目录下：

```bash
git clone --branch feat/symphony-provider https://github.com/Sarthakischauhan/zeron-symphony.git
git clone --branch feat/zeron-stdio-agent https://github.com/Sarthakischauhan/symphony.git
```

安装 Symphony 并构建 Zeron：

```bash
cd symphony
uv sync --all-packages
cd ../zeron-symphony
export SYMPHONY_EXECUTABLE="$(cd ../symphony && pwd)/.venv/bin/symphony"
cargo build --release --locked -p zeron
./target/release/zeron
```

请在启动 Zeron 的环境中设置模型提供商凭据。例如，Symphony 支持 `OPENAI_API_KEY`。更多配置、模型发现和当前集成限制，请参阅 [Symphony 安装指南](docs/symphony.md)。

Linux 需要安装 GPUI 的系统构建依赖。应用内浏览器还需要 [WebKitGTK 4.1 和 JSON-GLib](docs/reference/linux-browser.md)。

## 许可证

[MIT](LICENSE)
