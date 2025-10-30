# greentic-runner

Greentic runner host for executing agentic flows packaged as Wasm components.  
The workspace hosts the runner binary in `crates/host` plus integration tests in `crates/tests` and sample bindings under `examples/bindings/`.

## Getting Started

```bash
cargo run -p greentic-runner -- --pack /path/to/pack.wasm --bindings examples/bindings/default.bindings.yaml --port 8080
```

On startup the host loads the supplied pack, enumerates flows, and exposes the messaging webhook for Telegram under `/messaging/telegram/webhook`.

## License

MIT
