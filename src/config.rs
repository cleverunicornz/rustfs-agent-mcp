use std::env;

/// Runtime configuration, sourced exclusively from `RUSTFS_AGENT_MCP_*`
/// environment variables.
#[derive(Debug, Clone)]
pub struct Config {
    /// Address the MCP HTTP server binds to. Default `127.0.0.1`; production
    /// deployments set this to the host's Tailscale address.
    pub bind_host: String,
    pub bind_port: u16,
    /// S3/STS endpoint of the RustFS deployment (e.g. `https://blob.yeetz.cloud`).
    pub endpoint: String,
    pub region: String,
    /// Credentials of the scoped minter identity. Never the cluster root keys.
    pub access_key: String,
    pub secret_key: String,
    /// Every bucket this server manages must carry this prefix.
    pub bucket_prefix: String,
    /// Lifecycle retention (days) applied to newly created test buckets.
    /// `0` disables the lifecycle rule.
    pub retention_days: i32,
    /// Maximum object body size accepted and returned through tool calls.
    pub max_body_bytes: usize,
    /// Extra `Host` header values accepted by DNS-rebinding protection, in
    /// addition to `<bind_host>:<bind_port>` (e.g. the MagicDNS name).
    pub allowed_hosts: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("missing required environment variable {0}")]
    Missing(&'static str),
    #[error("invalid value for {variable}: {message}")]
    Invalid {
        variable: &'static str,
        message: String,
    },
}

impl Config {
    /// Reads configuration from the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|key| env::var(key).ok())
    }

    /// Testable constructor: `lookup` maps `RUSTFS_AGENT_MCP_*` names to values.
    pub fn from_lookup<F>(lookup: F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let var = |name: &'static str| -> Option<String> { lookup(name) };
        let required = |name: &'static str| -> Result<String, ConfigError> {
            var(name)
                .filter(|v| !v.trim().is_empty())
                .ok_or(ConfigError::Missing(name))
        };

        let endpoint = required("RUSTFS_AGENT_MCP_ENDPOINT")?;
        if !endpoint.starts_with("https://") && !endpoint.starts_with("http://") {
            return Err(ConfigError::Invalid {
                variable: "RUSTFS_AGENT_MCP_ENDPOINT",
                message: "must be an http:// or https:// URL".into(),
            });
        }

        let access_key = required("RUSTFS_AGENT_MCP_ACCESS_KEY")?;
        let secret_key = required("RUSTFS_AGENT_MCP_SECRET_KEY")?;

        let bind_host =
            var("RUSTFS_AGENT_MCP_BIND_HOST").unwrap_or_else(|| "127.0.0.1".to_string());
        let bind_port = parse_num(&lookup, "RUSTFS_AGENT_MCP_BIND_PORT", 8765u16, 1, 65535)?;
        let region = var("RUSTFS_AGENT_MCP_REGION").unwrap_or_else(|| "us-east-1".to_string());
        let bucket_prefix =
            var("RUSTFS_AGENT_MCP_BUCKET_PREFIX").unwrap_or_else(|| "agent-test-".to_string());
        if !bucket_prefix
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            || bucket_prefix.len() < 2
        {
            return Err(ConfigError::Invalid {
                variable: "RUSTFS_AGENT_MCP_BUCKET_PREFIX",
                message: "must be lowercase alphanumerics/hyphens, at least 2 chars".into(),
            });
        }
        let retention_days = parse_num(&lookup, "RUSTFS_AGENT_MCP_RETENTION_DAYS", 7i32, 0, 3650)?;
        let max_body_bytes = parse_num(
            &lookup,
            "RUSTFS_AGENT_MCP_MAX_BODY_BYTES",
            4 * 1024 * 1024usize,
            1024,
            64 * 1024 * 1024,
        )?;
        let allowed_hosts = var("RUSTFS_AGENT_MCP_ALLOWED_HOSTS")
            .map(|raw| {
                raw.split(',')
                    .map(str::trim)
                    .filter(|h| !h.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        Ok(Self {
            bind_host,
            bind_port,
            endpoint,
            region,
            access_key,
            secret_key,
            bucket_prefix,
            retention_days,
            max_body_bytes,
            allowed_hosts,
        })
    }

    /// Host header values for DNS-rebinding protection: every configured
    /// allowed host plus the literal bind address when it is not a wildcard.
    pub fn effective_allowed_hosts(&self) -> Vec<String> {
        let mut hosts = self.allowed_hosts.clone();
        let bind = format!("{}:{}", self.bind_host, self.bind_port);
        if self.bind_host != "0.0.0.0" && self.bind_host != "::" && !hosts.contains(&bind) {
            hosts.push(bind);
        }
        hosts
    }
}

fn parse_num<F, T>(
    lookup: &F,
    name: &'static str,
    default: T,
    min: T,
    max: T,
) -> Result<T, ConfigError>
where
    F: Fn(&str) -> Option<String>,
    T: std::str::FromStr + Ord + Copy + std::fmt::Display,
    T::Err: std::fmt::Display,
{
    let fallback = format!("expected a number between {min} and {max}");
    match lookup(name) {
        None => Ok(default),
        Some(raw) => {
            let parsed = raw.trim().parse::<T>().map_err(|e| ConfigError::Invalid {
                variable: name,
                message: format!("{fallback}: {e}"),
            })?;
            if parsed < min || parsed > max {
                return Err(ConfigError::Invalid {
                    variable: name,
                    message: fallback,
                });
            }
            Ok(parsed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg_from(pairs: &[(&'static str, &str)]) -> Result<Config, ConfigError> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_lookup(move |key| map.get(key).cloned())
    }

    fn base_pairs() -> Vec<(&'static str, &'static str)> {
        vec![
            ("RUSTFS_AGENT_MCP_ENDPOINT", "https://blob.example.com"),
            ("RUSTFS_AGENT_MCP_ACCESS_KEY", "ak"),
            ("RUSTFS_AGENT_MCP_SECRET_KEY", "sk"),
        ]
    }

    #[test]
    fn defaults_are_sane() {
        let cfg = cfg_from(&base_pairs()).expect("config");
        assert_eq!(cfg.bind_host, "127.0.0.1");
        assert_eq!(cfg.bind_port, 8765);
        assert_eq!(cfg.region, "us-east-1");
        assert_eq!(cfg.bucket_prefix, "agent-test-");
        assert_eq!(cfg.retention_days, 7);
        assert_eq!(cfg.max_body_bytes, 4 * 1024 * 1024);
    }

    #[test]
    fn requires_endpoint_and_credentials() {
        assert!(matches!(
            cfg_from(&[]),
            Err(ConfigError::Missing("RUSTFS_AGENT_MCP_ENDPOINT"))
        ));
        let mut pairs = base_pairs();
        pairs.retain(|(k, _)| *k != "RUSTFS_AGENT_MCP_ACCESS_KEY");
        assert!(matches!(
            cfg_from(&pairs),
            Err(ConfigError::Missing("RUSTFS_AGENT_MCP_ACCESS_KEY"))
        ));
    }

    #[test]
    fn rejects_bad_prefix_and_endpoint() {
        let mut pairs = base_pairs();
        pairs.push(("RUSTFS_AGENT_MCP_ENDPOINT", "blob.example.com"));
        assert!(matches!(cfg_from(&pairs), Err(ConfigError::Invalid { .. })));
        let mut pairs = base_pairs();
        pairs.push(("RUSTFS_AGENT_MCP_BUCKET_PREFIX", "Agent_Test"));
        assert!(matches!(cfg_from(&pairs), Err(ConfigError::Invalid { .. })));
    }

    #[test]
    fn rejects_out_of_range_numbers() {
        let mut pairs = base_pairs();
        pairs.push(("RUSTFS_AGENT_MCP_RETENTION_DAYS", "9999"));
        assert!(matches!(cfg_from(&pairs), Err(ConfigError::Invalid { .. })));
        let mut pairs = base_pairs();
        pairs.push(("RUSTFS_AGENT_MCP_BIND_PORT", "not-a-port"));
        assert!(matches!(cfg_from(&pairs), Err(ConfigError::Invalid { .. })));
    }

    #[test]
    fn allowed_hosts_include_bind_address_unless_wildcard() {
        let mut pairs = base_pairs();
        pairs.push(("RUSTFS_AGENT_MCP_ALLOWED_HOSTS", "dev1.example.ts.net:8765"));
        let cfg = cfg_from(&pairs).expect("config");
        let hosts = cfg.effective_allowed_hosts();
        assert!(hosts.contains(&"dev1.example.ts.net:8765".to_string()));
        assert!(hosts.contains(&"127.0.0.1:8765".to_string()));

        let mut pairs = base_pairs();
        pairs.push(("RUSTFS_AGENT_MCP_BIND_HOST", "0.0.0.0"));
        let cfg = cfg_from(&pairs).expect("config");
        assert!(
            !cfg.effective_allowed_hosts()
                .iter()
                .any(|h| h.starts_with("0.0.0.0"))
        );
    }
}
