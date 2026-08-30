use base64::Engine as _;
use rust_mcp_sdk::macros::{JsonSchema, mcp_tool};
use rust_mcp_sdk::schema::schema_utils::CallToolError;
use rust_mcp_sdk::schema::{CallToolResult, TextContent};
use rust_mcp_sdk::{McpServer, mcp_server::ServerHandler, tool_box};
use serde::Serialize;
use serde_json::json;
use std::sync::Arc;

use crate::config::Config;
use crate::policy::{TestBucketName, session_policy};

/// Shared state handed to every tool invocation.
pub struct ToolCtx {
    pub s3: aws_sdk_s3::Client,
    pub sts: aws_sdk_sts::Client,
    pub cfg: Config,
}

fn op_err(operation: &str, err: impl std::fmt::Display) -> CallToolError {
    CallToolError::from_message(format!("{operation} failed: {err}"))
}

fn json_out(value: &impl Serialize) -> Result<CallToolResult, CallToolError> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| CallToolError::from_message(format!("failed to encode result: {e}")))?;
    Ok(CallToolResult::text_content(vec![TextContent::from(text)]))
}

fn compose(bucket: &str, prefix: &str) -> Result<TestBucketName, CallToolError> {
    TestBucketName::compose(bucket, prefix)
        .map_err(|e| CallToolError::from_message(format!("bucket rejected: {e}")))
}

// ---------------------------------------------------------------- list_buckets

#[mcp_tool(
    name = "list_buckets",
    description = "List test buckets (names beginning with the configured agent-test prefix). \
                   Production buckets are never visible through this server.",
    title = "List test buckets",
    read_only_hint = true,
    idempotent_hint = true
)]
#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema)]
pub struct ListBucketsTool {}

impl ListBucketsTool {
    pub async fn run(&self, ctx: &ToolCtx) -> Result<CallToolResult, CallToolError> {
        let resp = ctx
            .s3
            .list_buckets()
            .send()
            .await
            .map_err(|e| op_err("list_buckets", e))?;
        let buckets: Vec<_> = resp
            .buckets()
            .iter()
            .filter_map(|b| {
                let name = b.name().unwrap_or_default();
                if name.starts_with(&ctx.cfg.bucket_prefix) {
                    Some(json!({
                        "name": name,
                        "creation_date": b.creation_date().map(|d| d.to_string()),
                    }))
                } else {
                    None
                }
            })
            .collect();
        json_out(&json!({ "prefix": ctx.cfg.bucket_prefix, "buckets": buckets }))
    }
}

// -------------------------------------------------------------- create_bucket

#[mcp_tool(
    name = "create_bucket",
    description = "Create a test bucket. Pass a bare name (the agent-test prefix is applied \
                   automatically) or omit it to get a generated unique name. A lifecycle rule \
                   expires objects after the configured retention window so abandoned buckets \
                   self-clean.",
    title = "Create test bucket"
)]
#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema)]
pub struct CreateBucketTool {
    /// Bare bucket name; the test prefix is prepended automatically.
    #[serde(default)]
    pub name: Option<String>,
}

impl CreateBucketTool {
    pub async fn run(&self, ctx: &ToolCtx) -> Result<CallToolResult, CallToolError> {
        let bucket = match &self.name {
            Some(raw) => compose(raw, &ctx.cfg.bucket_prefix)?,
            None => TestBucketName::generate(&ctx.cfg.bucket_prefix),
        };
        ctx.s3
            .create_bucket()
            .bucket(bucket.as_str())
            .send()
            .await
            .map_err(|e| op_err("create_bucket", e))?;

        let lifecycle = if ctx.cfg.retention_days > 0 {
            apply_retention(ctx, bucket.as_str(), ctx.cfg.retention_days).await
        } else {
            "disabled by configuration".to_string()
        };
        json_out(&json!({
            "bucket": bucket.as_str(),
            "retention": lifecycle,
        }))
    }
}

async fn apply_retention(ctx: &ToolCtx, bucket: &str, days: i32) -> String {
    use aws_sdk_s3::types::{
        BucketLifecycleConfiguration, ExpirationStatus, LifecycleExpiration, LifecycleRule,
        LifecycleRuleFilter,
    };
    let config = LifecycleRule::builder()
        .id("agent-test-retention")
        .status(ExpirationStatus::Enabled)
        .filter(LifecycleRuleFilter::builder().prefix("").build())
        .expiration(LifecycleExpiration::builder().days(days).build())
        .build()
        .ok()
        .and_then(|rule| {
            BucketLifecycleConfiguration::builder()
                .rules(rule)
                .build()
                .ok()
        });
    if let Some(config) = config {
        match ctx
            .s3
            .put_bucket_lifecycle_configuration()
            .bucket(bucket)
            .lifecycle_configuration(config)
            .send()
            .await
        {
            Ok(_) => format!("{days} day lifecycle rule applied"),
            Err(e) => format!(
                "lifecycle rule unavailable ({e}); bucket works, objects will not auto-expire"
            ),
        }
    } else {
        "lifecycle rule unavailable; bucket works, objects will not auto-expire".to_string()
    }
}

// -------------------------------------------------------------- delete_bucket

#[mcp_tool(
    name = "delete_bucket",
    description = "Delete a test bucket. With force=true all contained objects are deleted \
                   first; otherwise a non-empty bucket fails with BucketNotEmpty.",
    title = "Delete test bucket",
    destructive_hint = true
)]
#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema)]
pub struct DeleteBucketTool {
    /// Bucket name (with or without the test prefix).
    pub bucket: String,
    /// Delete every object in the bucket before removing it.
    #[serde(default)]
    pub force: Option<bool>,
}

const FORCE_DELETE_OBJECT_LIMIT: usize = 10_000;

impl DeleteBucketTool {
    pub async fn run(&self, ctx: &ToolCtx) -> Result<CallToolResult, CallToolError> {
        let bucket = compose(&self.bucket, &ctx.cfg.bucket_prefix)?;
        let mut deleted = 0usize;
        if self.force.unwrap_or(false) {
            let mut token: Option<String> = None;
            loop {
                let mut req = ctx
                    .s3
                    .list_objects_v2()
                    .bucket(bucket.as_str())
                    .max_keys(1000);
                if let Some(t) = &token {
                    req = req.continuation_token(t);
                }
                let page = req
                    .send()
                    .await
                    .map_err(|e| op_err("delete_bucket (list)", e))?;
                for obj in page.contents() {
                    if let Some(key) = obj.key() {
                        ctx.s3
                            .delete_object()
                            .bucket(bucket.as_str())
                            .key(key)
                            .send()
                            .await
                            .map_err(|e| op_err("delete_bucket (delete object)", e))?;
                        deleted += 1;
                        if deleted > FORCE_DELETE_OBJECT_LIMIT {
                            return Err(CallToolError::from_message(format!(
                                "refusing to force-delete more than {FORCE_DELETE_OBJECT_LIMIT} objects"
                            )));
                        }
                    }
                }
                token = page.next_continuation_token().map(str::to_string);
                if token.is_none() {
                    break;
                }
            }
        }
        ctx.s3
            .delete_bucket()
            .bucket(bucket.as_str())
            .send()
            .await
            .map_err(|e| op_err("delete_bucket", e))?;
        json_out(&json!({ "bucket": bucket.as_str(), "objects_deleted": deleted }))
    }
}

// -------------------------------------------------------------- list_objects

#[mcp_tool(
    name = "list_objects",
    description = "List objects in a test bucket, optionally filtered by key prefix.",
    title = "List objects",
    read_only_hint = true,
    idempotent_hint = true
)]
#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema)]
pub struct ListObjectsTool {
    pub bucket: String,
    /// Only keys beginning with this prefix.
    #[serde(default)]
    pub prefix: Option<String>,
    /// Page size, 1-1000 (default 100).
    #[serde(default)]
    pub max_keys: Option<i32>,
    /// Token from a previous response's next_continuation_token.
    #[serde(default)]
    pub continuation_token: Option<String>,
}

impl ListObjectsTool {
    pub async fn run(&self, ctx: &ToolCtx) -> Result<CallToolResult, CallToolError> {
        let bucket = compose(&self.bucket, &ctx.cfg.bucket_prefix)?;
        let max_keys = self.max_keys.unwrap_or(100).clamp(1, 1000);
        let mut req = ctx
            .s3
            .list_objects_v2()
            .bucket(bucket.as_str())
            .max_keys(max_keys);
        if let Some(p) = &self.prefix {
            req = req.prefix(p);
        }
        if let Some(t) = &self.continuation_token {
            req = req.continuation_token(t);
        }
        let resp = req.send().await.map_err(|e| op_err("list_objects", e))?;
        let objects: Vec<_> = resp
            .contents()
            .iter()
            .map(|o| {
                json!({
                    "key": o.key(),
                    "size": o.size(),
                    "etag": o.e_tag(),
                    "last_modified": o.last_modified().map(|m| m.to_string()),
                })
            })
            .collect();
        json_out(&json!({
            "bucket": bucket.as_str(),
            "objects": objects,
            "is_truncated": resp.is_truncated().unwrap_or(false),
            "next_continuation_token": resp.next_continuation_token(),
        }))
    }
}

// --------------------------------------------------------------- head_object

#[mcp_tool(
    name = "head_object",
    description = "Fetch object metadata (size, etag, content type, last modified) without \
                   downloading the body.",
    title = "Object metadata",
    read_only_hint = true,
    idempotent_hint = true
)]
#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema)]
pub struct HeadObjectTool {
    pub bucket: String,
    pub key: String,
}

impl HeadObjectTool {
    pub async fn run(&self, ctx: &ToolCtx) -> Result<CallToolResult, CallToolError> {
        let bucket = compose(&self.bucket, &ctx.cfg.bucket_prefix)?;
        let resp = ctx
            .s3
            .head_object()
            .bucket(bucket.as_str())
            .key(&self.key)
            .send()
            .await
            .map_err(|e| op_err("head_object", e))?;
        json_out(&json!({
            "bucket": bucket.as_str(),
            "key": self.key,
            "size": resp.content_length(),
            "etag": resp.e_tag(),
            "content_type": resp.content_type(),
            "last_modified": resp.last_modified().map(|m| m.to_string()),
        }))
    }
}

// ---------------------------------------------------------------- get_object

#[mcp_tool(
    name = "get_object",
    description = "Download an object. Returns UTF-8 text when the body is valid UTF-8, \
                   otherwise base64. Bodies larger than max_bytes are truncated and flagged.",
    title = "Get object",
    read_only_hint = true
)]
#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema)]
pub struct GetObjectTool {
    pub bucket: String,
    pub key: String,
    /// Maximum bytes to return (default 256 KiB, server-capped).
    #[serde(default)]
    pub max_bytes: Option<i64>,
}

impl GetObjectTool {
    pub async fn run(&self, ctx: &ToolCtx) -> Result<CallToolResult, CallToolError> {
        let bucket = compose(&self.bucket, &ctx.cfg.bucket_prefix)?;
        let max =
            (self.max_bytes.unwrap_or(256 * 1024)).clamp(1, ctx.cfg.max_body_bytes as i64) as usize;

        let head = ctx
            .s3
            .head_object()
            .bucket(bucket.as_str())
            .key(&self.key)
            .send()
            .await
            .map_err(|e| op_err("get_object (head)", e))?;
        let size = head.content_length().unwrap_or(0) as usize;
        let truncated = size > max;

        let mut req = ctx.s3.get_object().bucket(bucket.as_str()).key(&self.key);
        if truncated {
            req = req.range(format!("bytes=0-{}", max - 1));
        }
        let resp = req.send().await.map_err(|e| op_err("get_object", e))?;
        let content_type = resp.content_type().map(str::to_string);
        let etag = resp.e_tag().map(str::to_string);
        let bytes = resp
            .body
            .collect()
            .await
            .map_err(|e| op_err("get_object (body)", e))?
            .into_bytes();

        let (encoding, content) = match std::str::from_utf8(&bytes) {
            Ok(text) => ("utf8", text.to_string()),
            Err(_) => (
                "base64",
                base64::engine::general_purpose::STANDARD.encode(&bytes),
            ),
        };
        json_out(&json!({
            "bucket": bucket.as_str(),
            "key": self.key,
            "encoding": encoding,
            "content": content,
            "size": size,
            "returned_bytes": bytes.len(),
            "truncated": truncated,
            "content_type": content_type,
            "etag": etag,
        }))
    }
}

// ---------------------------------------------------------------- put_object

#[mcp_tool(
    name = "put_object",
    description = "Upload an object. Provide content as plain text or base64 (exactly one).",
    title = "Put object"
)]
#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema)]
pub struct PutObjectTool {
    pub bucket: String,
    pub key: String,
    /// Plain-text content (mutually exclusive with content_base64).
    #[serde(default)]
    pub content: Option<String>,
    /// Base64-encoded binary content (mutually exclusive with content).
    #[serde(default)]
    pub content_base64: Option<String>,
    /// Optional content type stored with the object.
    #[serde(default)]
    pub content_type: Option<String>,
}

impl PutObjectTool {
    pub async fn run(&self, ctx: &ToolCtx) -> Result<CallToolResult, CallToolError> {
        let bucket = compose(&self.bucket, &ctx.cfg.bucket_prefix)?;
        let bytes: Vec<u8> = match (&self.content, &self.content_base64) {
            (Some(text), None) => text.clone().into_bytes(),
            (None, Some(b64)) => base64::engine::general_purpose::STANDARD
                .decode(b64.as_bytes())
                .map_err(|e| CallToolError::from_message(format!("invalid base64: {e}")))?,
            _ => {
                return Err(CallToolError::from_message(
                    "provide exactly one of content or content_base64",
                ));
            }
        };
        if bytes.len() > ctx.cfg.max_body_bytes {
            return Err(CallToolError::from_message(format!(
                "body of {} bytes exceeds the {} byte limit",
                bytes.len(),
                ctx.cfg.max_body_bytes
            )));
        }
        let mut req = ctx
            .s3
            .put_object()
            .bucket(bucket.as_str())
            .key(&self.key)
            .body(aws_sdk_s3::primitives::ByteStream::from(bytes.clone()));
        if let Some(ct) = &self.content_type {
            req = req.content_type(ct);
        }
        let resp = req.send().await.map_err(|e| op_err("put_object", e))?;
        json_out(&json!({
            "bucket": bucket.as_str(),
            "key": self.key,
            "size": bytes.len(),
            "etag": resp.e_tag(),
        }))
    }
}

// ------------------------------------------------------------- delete_object

#[mcp_tool(
    name = "delete_object",
    description = "Delete a single object from a test bucket.",
    title = "Delete object",
    destructive_hint = true,
    idempotent_hint = true
)]
#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema)]
pub struct DeleteObjectTool {
    pub bucket: String,
    pub key: String,
}

impl DeleteObjectTool {
    pub async fn run(&self, ctx: &ToolCtx) -> Result<CallToolResult, CallToolError> {
        let bucket = compose(&self.bucket, &ctx.cfg.bucket_prefix)?;
        ctx.s3
            .delete_object()
            .bucket(bucket.as_str())
            .key(&self.key)
            .send()
            .await
            .map_err(|e| op_err("delete_object", e))?;
        json_out(&json!({ "bucket": bucket.as_str(), "key": self.key, "deleted": true }))
    }
}

// --------------------------------------------------------------- presign_url

#[mcp_tool(
    name = "presign_url",
    description = "Mint a presigned URL for direct HTTP access to an object, bypassing this \
                   server for bulk transfers. Works for objects that do not exist yet when \
                   method=put.",
    title = "Presign URL"
)]
#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema)]
pub struct PresignUrlTool {
    pub bucket: String,
    pub key: String,
    /// "get" (default) or "put".
    #[serde(default)]
    pub method: Option<String>,
    /// URL lifetime in seconds (default 3600, max 604800).
    #[serde(default)]
    pub ttl_seconds: Option<i64>,
}

impl PresignUrlTool {
    pub async fn run(&self, ctx: &ToolCtx) -> Result<CallToolResult, CallToolError> {
        use aws_sdk_s3::presigning::PresigningConfig;
        let bucket = compose(&self.bucket, &ctx.cfg.bucket_prefix)?;
        let method = self.method.clone().unwrap_or_else(|| "get".to_string());
        let ttl = self.ttl_seconds.unwrap_or(3600).clamp(60, 604_800);
        let config = PresigningConfig::expires_in(std::time::Duration::from_secs(ttl as u64))
            .map_err(|e| op_err("presign", e))?;
        let uri = match method.as_str() {
            "get" => ctx
                .s3
                .get_object()
                .bucket(bucket.as_str())
                .key(&self.key)
                .presigned(config)
                .await
                .map_err(|e| op_err("presign get", e))?,
            "put" => ctx
                .s3
                .put_object()
                .bucket(bucket.as_str())
                .key(&self.key)
                .presigned(config)
                .await
                .map_err(|e| op_err("presign put", e))?,
            other => {
                return Err(CallToolError::from_message(format!(
                    "method must be \"get\" or \"put\", not {other:?}"
                )));
            }
        };
        json_out(&json!({
            "bucket": bucket.as_str(),
            "key": self.key,
            "method": method,
            "url": uri.uri().to_string(),
            "ttl_seconds": ttl,
        }))
    }
}

// ----------------------------------------------------------- mint_credentials

#[mcp_tool(
    name = "mint_credentials",
    description = "Mint ephemeral, auto-expiring S3 credentials for test code via STS. Scoped \
                   to specific test buckets when `buckets` is given, otherwise to the whole \
                   agent-test namespace (including bucket listing). Credentials expire \
                   automatically; nothing needs revoking. Pass them to tests as \
                   AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY / AWS_SESSION_TOKEN with \
                   AWS_ENDPOINT_URL set to the returned endpoint.",
    title = "Mint ephemeral credentials"
)]
#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema)]
pub struct MintCredentialsTool {
    /// Restrict the credentials to these test buckets (default: whole test namespace).
    #[serde(default)]
    pub buckets: Option<Vec<String>>,
    /// Credential lifetime in minutes (default 60, min 15, max 720).
    #[serde(default)]
    pub ttl_minutes: Option<i32>,
}

impl MintCredentialsTool {
    pub async fn run(&self, ctx: &ToolCtx) -> Result<CallToolResult, CallToolError> {
        let ttl_minutes = self.ttl_minutes.unwrap_or(60);
        if !(15..=720).contains(&ttl_minutes) {
            return Err(CallToolError::from_message(
                "ttl_minutes must be between 15 and 720",
            ));
        }
        let scope: Option<Vec<String>> = match &self.buckets {
            Some(names) if names.is_empty() => None,
            Some(names) => {
                let mut resolved = Vec::with_capacity(names.len());
                for name in names {
                    resolved.push(compose(name, &ctx.cfg.bucket_prefix)?.to_string());
                }
                Some(resolved)
            }
            None => None,
        };
        let policy_json = session_policy(scope.as_deref(), &ctx.cfg.bucket_prefix).to_string();
        let session = format!("agent-mcp-{}", uuid::Uuid::new_v4().simple());
        let resp = ctx
            .sts
            .assume_role()
            .role_arn("arn:aws:iam:::role/agent-mcp")
            .role_session_name(session)
            .policy(policy_json)
            .duration_seconds(ttl_minutes * 60)
            .send()
            .await
            .map_err(|e| op_err("mint_credentials (assume_role)", e))?;
        let creds = resp
            .credentials()
            .ok_or_else(|| CallToolError::from_message("STS returned no credentials"))?;

        json_out(&json!({
            "endpoint": ctx.cfg.endpoint,
            "region": ctx.cfg.region,
            "access_key_id": creds.access_key_id(),
            "secret_access_key": creds.secret_access_key(),
            "session_token": creds.session_token(),
            "expires_at": creds.expiration().to_string(),
            "scope": match &scope {
                Some(buckets) => json!({ "buckets": buckets }),
                None => json!({ "prefix": format!("{}*", ctx.cfg.bucket_prefix) }),
            },
            "note": "credentials expire on their own; use AWS_SESSION_TOKEN alongside the key pair",
        }))
    }
}

// --------------------------------------------------------------------- wiring

tool_box!(
    StorageTools,
    [
        ListBucketsTool,
        CreateBucketTool,
        DeleteBucketTool,
        ListObjectsTool,
        HeadObjectTool,
        GetObjectTool,
        PutObjectTool,
        DeleteObjectTool,
        PresignUrlTool,
        MintCredentialsTool,
    ]
);

/// The MCP server handler: routes tool calls onto [`ToolCtx`].
pub struct StorageToolsHandler {
    pub ctx: Arc<ToolCtx>,
}

#[async_trait::async_trait]
impl ServerHandler for StorageToolsHandler {
    async fn handle_list_tools_request(
        &self,
        _params: Option<rust_mcp_sdk::schema::PaginatedRequestParams>,
        _runtime: Arc<dyn McpServer>,
    ) -> std::result::Result<rust_mcp_sdk::schema::ListToolsResult, rust_mcp_sdk::schema::RpcError>
    {
        Ok(rust_mcp_sdk::schema::ListToolsResult {
            meta: None,
            next_cursor: None,
            tools: StorageTools::tools(),
        })
    }

    async fn handle_call_tool_request(
        &self,
        params: rust_mcp_sdk::schema::CallToolRequestParams,
        _runtime: Arc<dyn McpServer>,
    ) -> Result<CallToolResult, CallToolError> {
        let tool: StorageTools = StorageTools::try_from(params).map_err(CallToolError::new)?;
        match tool {
            StorageTools::ListBucketsTool(t) => t.run(&self.ctx).await,
            StorageTools::CreateBucketTool(t) => t.run(&self.ctx).await,
            StorageTools::DeleteBucketTool(t) => t.run(&self.ctx).await,
            StorageTools::ListObjectsTool(t) => t.run(&self.ctx).await,
            StorageTools::HeadObjectTool(t) => t.run(&self.ctx).await,
            StorageTools::GetObjectTool(t) => t.run(&self.ctx).await,
            StorageTools::PutObjectTool(t) => t.run(&self.ctx).await,
            StorageTools::DeleteObjectTool(t) => t.run(&self.ctx).await,
            StorageTools::PresignUrlTool(t) => t.run(&self.ctx).await,
            StorageTools::MintCredentialsTool(t) => t.run(&self.ctx).await,
        }
    }
}
