use serde_json::{Value, json};

/// An S3 bucket name composed from the configured test prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestBucketName(String);

impl TestBucketName {
    /// Accepts either a bare suffix (the server prefix is prepended) or a
    /// full name that already carries the prefix. Anything outside the test
    /// namespace is rejected.
    pub fn compose(bucket: &str, prefix: &str) -> Result<Self, String> {
        let full = if bucket.starts_with(prefix) {
            bucket.to_string()
        } else {
            format!("{prefix}{bucket}")
        };
        if !is_valid_bucket_name(&full) {
            return Err(format!(
                "'{full}' is not a valid S3 bucket name (3-63 chars, lowercase \
                 alphanumeric, hyphen or dot separators, no '..' segments)"
            ));
        }
        Ok(Self(full))
    }

    /// Generates a fresh, collision-resistant name inside the namespace.
    pub fn generate(prefix: &str) -> Self {
        Self(format!("{prefix}{}", uuid::Uuid::new_v4().simple()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TestBucketName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Validates the composed name against S3 bucket naming rules.
pub fn is_valid_bucket_name(name: &str) -> bool {
    let len = name.len();
    if !(3..=63).contains(&len) {
        return false;
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.')
    {
        return false;
    }
    if name.contains("..") || name.contains(".-") || name.contains("-.") {
        return false;
    }
    let first = name.chars().next().expect("len >= 3");
    let last = name.chars().nth_back(0).expect("len >= 3");
    let ok_first = first.is_ascii_lowercase() || first.is_ascii_digit();
    let ok_last = last.is_ascii_lowercase() || last.is_ascii_digit();
    ok_first && ok_last
}

/// Builds the STS session policy that constrains minted credentials.
///
/// `Some(buckets)` scopes the session to exactly those test buckets (tightest
/// scope: no bucket listing). `None` scopes it to the entire test namespace
/// prefix, including bucket listing so test harnesses can discover buckets.
pub fn session_policy(buckets: Option<&[String]>, prefix: &str) -> Value {
    let mut statements = Vec::new();
    if buckets.is_none() {
        statements.push(json!({
            "Effect": "Allow",
            "Action": ["s3:ListAllMyBuckets"],
            "Resource": ["arn:aws:s3:::*"]
        }));
    }
    let resources: Vec<String> = match buckets {
        Some(names) => names
            .iter()
            .flat_map(|n| [format!("arn:aws:s3:::{n}"), format!("arn:aws:s3:::{n}/*")])
            .collect(),
        None => vec![
            format!("arn:aws:s3:::{prefix}*"),
            format!("arn:aws:s3:::{prefix}*/*"),
        ],
    };
    statements.push(json!({
        "Effect": "Allow",
        "Action": ["s3:*"],
        "Resource": resources
    }));
    json!({
        "Version": "2012-10-17",
        "Statement": statements
    })
}

/// The IAM policy document the minter identity itself carries. It can only
/// see bucket names and touch objects inside the test namespace.
pub fn minter_policy(prefix: &str) -> Value {
    json!({
        "Version": "2012-10-17",
        "Statement": [
            {
                "Effect": "Allow",
                "Action": ["s3:ListAllMyBuckets"],
                "Resource": ["arn:aws:s3:::*"]
            },
            {
                "Effect": "Allow",
                "Action": ["s3:*"],
                "Resource": [
                    format!("arn:aws:s3:::{prefix}*"),
                    format!("arn:aws:s3:::{prefix}*/*")
                ]
            }
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PREFIX: &str = "agent-test-";

    #[test]
    fn compose_prepends_prefix_or_accepts_prefixed() {
        assert_eq!(
            TestBucketName::compose("foo", PREFIX).unwrap().as_str(),
            "agent-test-foo"
        );
        assert_eq!(
            TestBucketName::compose("agent-test-foo", PREFIX)
                .unwrap()
                .as_str(),
            "agent-test-foo"
        );
    }

    #[test]
    fn compose_rejects_names_that_leave_the_namespace_or_are_invalid() {
        assert!(TestBucketName::compose("UPPER", PREFIX).is_err());
        assert!(TestBucketName::compose(&"x".repeat(80), PREFIX).is_err());
        assert!(TestBucketName::compose("a..b", PREFIX).is_err());
        // Short bare suffixes are rescued by the prefix.
        assert!(TestBucketName::compose("ab", PREFIX).is_ok());
    }

    #[test]
    fn generated_names_are_valid_and_prefixed() {
        for _ in 0..32 {
            let name = TestBucketName::generate(PREFIX);
            assert!(is_valid_bucket_name(name.as_str()));
            assert!(name.as_str().starts_with(PREFIX));
        }
    }

    #[test]
    fn session_policy_for_specific_buckets_is_tight() {
        let policy = session_policy(
            Some(&[
                "agent-test-alpha".to_string(),
                "agent-test-beta".to_string(),
            ]),
            PREFIX,
        );
        let text = policy.to_string();
        assert!(!text.contains("ListAllMyBuckets"));
        assert!(text.contains("arn:aws:s3:::agent-test-alpha"));
        assert!(text.contains("arn:aws:s3:::agent-test-beta/*"));
    }

    #[test]
    fn session_policy_for_namespace_allows_listing() {
        let policy = session_policy(None, PREFIX);
        let text = policy.to_string();
        assert!(text.contains("ListAllMyBuckets"));
        assert!(text.contains("arn:aws:s3:::agent-test-*"));
        assert!(text.contains("arn:aws:s3:::agent-test-*/*"));
    }

    #[test]
    fn minter_policy_never_grants_out_of_namespace_object_access() {
        let policy = minter_policy(PREFIX);
        let text = policy.to_string();
        assert!(!text.contains("Resource\":[\"*\"]"));
        assert!(text.contains("arn:aws:s3:::agent-test-*"));
    }
}
