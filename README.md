# basalt-plugin-gemini-agent

Basalt plugin: Google Gemini AI coding agent launcher.

## Installation

Download the latest `.wasm` from [Releases](https://github.com/adevcorn/basalt-plugin-gemini-agent/releases) and place it in `~/.config/basalt/plugins/`.

Or install via the Basalt plugin registry.

## Building from source

```bash
rustup target add wasm32-unknown-unknown
cargo build --target wasm32-unknown-unknown --release
cp target/wasm32-unknown-unknown/release/gemini_agent.wasm ~/.config/basalt/plugins/
```
