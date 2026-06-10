# acp-proxy

Transparent recording proxy for [ACP][acp] agents.

[acp]: https://agentclientprotocol.com/

## How it works

The proxy sits between an ACP client and an ACP agent:

```
Client ──stdin──▶ [acp-proxy] ──stdin──▶ Agent
       ◀──stdout──           ◀──stdout──
```

Every frame crossing the boundary is:
1. Parsed and classified
2. Written to a trace directory (via `acp-trace`)
3. Forwarded with original bytes intact

The agent does not need modification — bytes are forwarded untouched.

## Architecture

Three OS threads handle the I/O:
- `parent_stdin → child_stdin` (C2A, recording)
- `child_stdout → parent_stdout` (A2C, recording)
- `child_stderr → parent_stderr` (passthrough, no recording)

No async runtime. No tokio. This is the minimum correct shape for a stdio MITM.

## Usage

```rust
use acp_proxy::{run, ProxyConfig};
use std::path::PathBuf;

let config = ProxyConfig {
    agent_argv: vec!["./my-agent".into()],
    trace_dir: PathBuf::from("./trace"),
    recorder_version: "0.1.0".into(),
    env_whitelist: vec!["PATH".into()],  // only these env vars are captured
    skip_recording_env: false,
    clock: None,  // use system clock
};

let exit_code = run(
    config,
    std::io::stdin(),
    std::io::stdout(),
    std::io::stderr(),
)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Security

Environment variables are **not** captured by default. Only keys explicitly listed in `env_whitelist` are stored in the manifest. This prevents accidental leakage of `*_API_KEY`, `*_TOKEN`, etc.

## License

Apache-2.0
