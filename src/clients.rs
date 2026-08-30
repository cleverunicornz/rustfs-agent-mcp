use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;

use crate::config::Config;

fn credentials(cfg: &Config) -> Credentials {
    Credentials::new(
        cfg.access_key.clone(),
        cfg.secret_key.clone(),
        None,
        None,
        "rustfs-agent-mcp-minter",
    )
}

/// S3 client wired to the RustFS endpoint. Path-style addressing is mandatory:
/// the public certificate covers only the endpoint hostname, not
/// `<bucket>.<endpoint>` virtual-host names.
pub fn s3_client(cfg: &Config) -> aws_sdk_s3::Client {
    aws_sdk_s3::Client::from_conf(
        aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new(cfg.region.clone()))
            .endpoint_url(cfg.endpoint.clone())
            .credentials_provider(credentials(cfg))
            .force_path_style(true)
            .request_checksum_calculation(
                aws_sdk_s3::config::RequestChecksumCalculation::WhenRequired,
            )
            .response_checksum_validation(
                aws_sdk_s3::config::ResponseChecksumValidation::WhenRequired,
            )
            .build(),
    )
}

/// STS client against the same endpoint; `assume_role` with an inline session
/// policy is how scoped, auto-expiring credentials are minted.
pub fn sts_client(cfg: &Config) -> aws_sdk_sts::Client {
    aws_sdk_sts::Client::from_conf(
        aws_sdk_sts::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(aws_sdk_sts::config::Region::new(cfg.region.clone()))
            .endpoint_url(cfg.endpoint.clone())
            .credentials_provider(credentials(cfg))
            .build(),
    )
}
