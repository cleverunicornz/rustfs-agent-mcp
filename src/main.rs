// The tool_box! macro generates an enum whose variants all end in `Tool` by
// design; that is the SDK's dispatch pattern, not a naming smell.
#![allow(clippy::enum_variant_names)]

mod clients;
mod config;
mod policy;
mod tools;

use anyhow::{Context, Result};
use rust_mcp_axum::{AxumServerOptions, create_axum_server};
use rust_mcp_sdk::TransportOptions;
use rust_mcp_sdk::event_store::InMemoryEventStore;
use rust_mcp_sdk::mcp_server::ToMcpServerHandler;
use rust_mcp_sdk::schema::{
    Implementation, InitializeResult, ProtocolVersion, ServerCapabilities, ServerCapabilitiesTools,
};
use rust_mcp_sdk::task_store::InMemoryTaskStore;
use std::sync::Arc;
use tools::{StorageToolsHandler, ToolCtx};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    // `print-minter-policy` emits the IAM policy document the scoped minter
    // identity must carry on RustFS. Deployment tooling consumes this output
    // verbatim; running the server itself never needs it.
    if std::env::args().nth(1).as_deref() == Some("print-minter-policy") {
        let prefix = std::env::var("RUSTFS_AGENT_MCP_BUCKET_PREFIX")
            .unwrap_or_else(|_| "agent-test-".to_string());
        println!("{}", policy::minter_policy(&prefix));
        return Ok(());
    }

    let cfg = config::Config::from_env().context("invalid configuration")?;
    let ctx = Arc::new(ToolCtx {
        s3: clients::s3_client(&cfg),
        sts: clients::sts_client(&cfg),
        cfg: cfg.clone(),
    });

    let server_details = InitializeResult {
        server_info: Implementation {
            name: "rustfs-agent-mcp".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            title: Some("RustFS agent storage MCP".into()),
            description: Some(
                "Agent-facing S3 surface over a RustFS cluster, restricted to the \
                 ephemeral agent-test namespace: bucket/object CRUD, presigned URLs, \
                 and auto-expiring scoped credential minting."
                    .into(),
            ),
            icons: vec![],
            website_url: Some("https://github.com/cleverunicornz/rustfs-agent-mcp".into()),
        },
        capabilities: ServerCapabilities {
            tools: Some(ServerCapabilitiesTools { list_changed: None }),
            ..Default::default()
        },
        meta: None,
        instructions: Some(format!(
            "All operations are restricted to buckets named '{}*'. Use \
             mint_credentials to obtain scoped, auto-expiring keys for test code \
             instead of handling long-lived secrets.",
            cfg.bucket_prefix
        )),
        protocol_version: ProtocolVersion::V2025_11_25.into(),
    };

    let allowed_hosts = cfg.effective_allowed_hosts();
    if allowed_hosts.is_empty() {
        anyhow::bail!(
            "DNS-rebinding protection requires at least one allowed host; set \
             RUSTFS_AGENT_MCP_ALLOWED_HOSTS when binding to a wildcard address"
        );
    }
    tracing::info!(
        endpoint = %cfg.endpoint,
        prefix = %cfg.bucket_prefix,
        bind = format!("{}:{}", cfg.bind_host, cfg.bind_port),
        allowed_hosts = allowed_hosts.join(","),
        "starting rustfs-agent-mcp"
    );

    let server = create_axum_server(
        server_details,
        StorageToolsHandler { ctx }.to_mcp_server_handler(),
        AxumServerOptions {
            host: cfg.bind_host.clone(),
            port: cfg.bind_port,
            event_store: Some(Arc::new(InMemoryEventStore::default())),
            task_store: Some(Arc::new(InMemoryTaskStore::new(None))),
            client_task_store: Some(Arc::new(InMemoryTaskStore::new(None))),
            transport_options: Arc::new(TransportOptions::default()),
            health_endpoint: Some("/health".into()),
            max_request_body_size: Some(cfg.max_body_bytes * 2),
            dns_rebinding: rust_mcp_sdk::mcp_http::DnsRebindingOptions {
                allowed_hosts: Some(allowed_hosts),
                ..Default::default()
            },
            ..Default::default()
        },
    );

    server
        .start()
        .await
        .map_err(|e| anyhow::anyhow!("MCP server failed: {e}"))?;
    Ok(())
}
