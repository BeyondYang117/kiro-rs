//! Regions are interpolated into credential-bearing endpoint URLs.

#[derive(Debug)]
pub(crate) struct InvalidRegion(pub &'static str);

impl std::fmt::Display for InvalidRegion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Do not echo the supplied value: it may contain credentials or controls.
        write!(
            f,
            "{} 必须是 1–63 字节的小写字母、数字或连字符，首尾须为字母或数字",
            self.0
        )
    }
}

impl std::error::Error for InvalidRegion {}

/// Accept one DNS label without hard-coding the provider's changing region list.
pub(crate) fn validate(field: &'static str, value: &str) -> Result<(), InvalidRegion> {
    let bytes = value.as_bytes();
    let alphanumeric = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    if !(1..=63).contains(&bytes.len())
        || !bytes.first().is_some_and(alphanumeric)
        || !bytes.last().is_some_and(alphanumeric)
        || !bytes.iter().all(|b| alphanumeric(b) || *b == b'-')
    {
        return Err(InvalidRegion(field));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_cannot_change_url_authority_path_or_query() {
        for value in [
            "",
            "-us-east-1",
            "us-east-1-",
            "US-EAST-1",
            "us_east_1",
            "us-east-1.evil.invalid",
            "evil.invalid/",
            "evil.invalid?secret=",
            "evil.invalid#",
            "user@evil.invalid/",
            "localhost:1234/",
            "us-east-1\\evil",
            "us-east-1%2f",
            "us-east-1\r\n",
            " us-east-1",
            "区域",
            &"a".repeat(64),
        ] {
            let error = validate("authRegion", value).unwrap_err();
            assert_eq!(error.0, "authRegion");
            assert!(!error.to_string().contains("evil"));
        }
        for value in [
            "us-east-1",
            "eu-central-1",
            "us-gov-west-1",
            "a",
            "1",
            &"a".repeat(63),
        ] {
            validate("region", value).unwrap();
            let url =
                reqwest::Url::parse(&format!("https://oidc.{value}.amazonaws.com/token")).unwrap();
            assert_eq!(
                url.host_str().unwrap(),
                format!("oidc.{value}.amazonaws.com")
            );
            assert_eq!(url.path(), "/token");
            assert!(url.query().is_none());
            assert!(url.username().is_empty());
        }
    }
}
