# rustfs-agent-mcp

An MCP server exposing a RustFS (S3-compatible) cluster to agents over
Streamable HTTP, restricted to an ephemeral test namespace.

Built with [`rust-mcp-sdk`](https://crates.io/crates/rust-mcp-sdk) and
[`rust-mcp-axum`](https://crates.io/crates/rust-mcp-axum). Deployed by the
[`cleverunicornz/infrastructure`](https://github.com/cleverunicornz/infrastructure)
repository; this repository only builds and releases the binary.

## Why

Agents and test harnesses need object storage, but handing every host a
long-lived cluster credential is wrong, and the official `rustfs-mcp` cannot
mint credentials. This server gives agents exactly the lifecycle they need:

1. create a scratch bucket,
2. mint scoped, **auto-expiring** STS credentials for test code,
3. do ordinary S3 CRUD and presigned URLs through tools,
4. delete the bucket when finished.

Nothing needs revoking — minted credentials expire on their own, and every
operation is confined to buckets whose names carry the configured test
prefix (`agent-test-` by default). Production buckets are unreachable through
this server by construction: the minter identity's IAM policy only grants
access inside the namespace, and the server additionally enforces the prefix
on every call.

## Tools

| Tool | Purpose |
| --- | --- |
| `list_buckets` | List test-namespace buckets only |
| `create_bucket` | Create a test bucket (auto-named or explicit); applies a lifecycle retention rule |
| `delete_bucket` | Delete a bucket, `force=true` empties it first |
| `list_objects` | Page through keys with sizes/etags |
| `head_object` | Object metadata |
| `get_object` | Download (UTF-8 or base64, size-capped, truncation flagged) |
| `put_object` | Upload (text or base64) |
| `delete_object` | Delete one key |
| `presign_url` | Direct GET/PUT URL for bulk transfers |
| `mint_credentials` | STS credentials scoped to given buckets (or the whole test namespace), 15–720 minutes, self-expiring |

## Configuration

All configuration is environment variables, prefix `RUSTFS_AGENT_MCP_`:

| Variable | Default | Meaning |
| --- | --- | --- |
| `ENDPOINT` | required | S3/STS endpoint, e.g. `https://blob.yeetz.cloud` |
| `ACCESS_KEY` / `SECRET_KEY` | required | The scoped minter identity — never cluster root keys |
| `BIND_HOST` | `127.0.0.1` | Bind address (production: the host's Tailscale address) |
| `BIND_PORT` | `8765` | Listen port |
| `REGION` | `us-east-1` | S3 region |
| `BUCKET_PREFIX` | `agent-test-` | Namespace enforced on every operation |
| `RETENTION_DAYS` | `7` | Lifecycle expiry for new buckets (`0` disables) |
| `MAX_BODY_BYTES` | `4194304` | Object body cap through tool calls |
| `ALLOWED_HOSTS` | bind address | Extra `Host` values for DNS-rebinding protection (e.g. the MagicDNS name) |

Endpoints: `/mcp` (Streamable HTTP), `/sse` + `/messages` (legacy SSE),
`/health` (liveness). Authentication is delegated to the network layer —
the server is meant to run inside a restricted segment (Tailscale ACLs), not
on the public internet.

### Minter policy

`rustfs-agent-mcp print-minter-policy` prints the exact IAM policy document
the minter identity must carry on the RustFS cluster. Deployment automation
consumes this output so the server and the IAM state can never drift apart.

## Build

```bash
cargo build --release --locked
cargo test --locked
cargo clippy --all-targets -- -D warnings
```

Releases are tagged `v*`; CI publishes `rustfs-agent-mcp-linux-amd64.tar.gz`
plus its SHA-256 as a GitHub Release artifact.

## License

MIT
