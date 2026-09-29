# Zeron Symphony

Zeron's desktop UI with the [Symphony coding agent](https://github.com/Sarthakischauhan/symphony) as its runtime. Zeron provides the workspace, chat, and agent UI; Symphony runs the model and tools.

*English | [简体中文](README.zh-CN.md)*

![Zeron desktop screenshot with a live coding session](apps/landing/public/assets/app-screenshot.jpg)

## Install from source

You need Python 3.11+, [uv](https://docs.astral.sh/uv/), and the stable Rust toolchain. Configure at least one model provider in Symphony before launching Zeron.

Clone both feature branches next to each other:

```bash
git clone --branch feat/symphony-provider https://github.com/Sarthakischauhan/zeron-symphony.git
git clone --branch feat/zeron-stdio-agent https://github.com/Sarthakischauhan/symphony.git
```

Install Symphony and build Zeron:

```bash
cd symphony
uv sync --all-packages
cd ../zeron-symphony
export SYMPHONY_EXECUTABLE="$(cd ../symphony && pwd)/.venv/bin/symphony"
cargo build --release --locked -p zeron
./target/release/zeron
```

Set your provider credentials in the environment used to launch Zeron. For example, Symphony supports `OPENAI_API_KEY`; see the [Symphony setup guide](docs/symphony.md) for details, model discovery, and current integration limits.

On Linux, install the system build dependencies for GPUI. The in-app browser also needs [WebKitGTK 4.1 and JSON-GLib](docs/reference/linux-browser.md).

## License

[MIT](LICENSE)
