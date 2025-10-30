# greentic-runner

Greentic runner host for executing agentic flows packaged as Wasm components.  
This repository contains the host binary (`crates/host`) and integration test scaffolding (`crates/tests`).

## Getting Started

```bash
cargo run -p host -- --pack /path/to/pack.wasm --bindings examples/bindings/default.bindings.yaml --port 8080
```

On startup the host loads the supplied pack, enumerates flows, and exposes the messaging webhook for Telegram under `/messaging/telegram/webhook`.

## License

MIT
