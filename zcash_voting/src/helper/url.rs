//! Canonical helper-server identity.

use url::Url;

use crate::types::VotingError;

/// Returns the stable base URL used for helper identity and persistence.
///
/// Helper bases may include a mount path, but not credentials, a query, or a
/// fragment. The returned form has a normalized origin, no default port, and
/// no trailing slash, so equivalent configuration spellings cannot bypass
/// delivery-history checks.
pub fn canonicalize_helper_base_url(value: &str) -> Result<String, VotingError> {
    let trimmed = value.trim();
    let mut url = Url::parse(trimmed).map_err(|error| VotingError::InvalidInput {
        message: format!("invalid helper server url {trimmed:?}: {error}"),
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(VotingError::InvalidInput {
            message: format!("helper server url must be http or https, got {trimmed}"),
        });
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(VotingError::InvalidInput {
            message: "helper server url must not contain credentials".to_string(),
        });
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(VotingError::InvalidInput {
            message: "helper server url must not contain a query or fragment".to_string(),
        });
    }

    let default_port = match url.scheme() {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    };
    if url.port() == default_port {
        url.set_port(None).map_err(|_| VotingError::InvalidInput {
            message: format!("invalid helper server port in {trimmed}"),
        })?;
    }

    // Query and fragment components are forbidden above, so trimming the
    // serialized suffix cannot alter anything except redundant path slashes.
    Ok(url.as_str().trim_end_matches('/').to_string())
}

/// Canonicalizes a helper URL list, deduplicating equivalent spellings.
///
/// # Errors
///
/// Returns [`VotingError::InvalidInput`] when any entry fails
/// [`canonicalize_helper_base_url`]; nothing is silently dropped.
pub fn canonical_helper_url_list(urls: &[String]) -> Result<Vec<String>, VotingError> {
    let mut canonical = Vec::with_capacity(urls.len());
    for url in urls {
        let url = canonicalize_helper_base_url(url)?;
        if !canonical.contains(&url) {
            canonical.push(url);
        }
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_equivalent_helper_bases() {
        assert_eq!(
            canonicalize_helper_base_url(" HTTPS://Helper.Example:443/vote/// ").unwrap(),
            "https://helper.example/vote"
        );
        assert_eq!(
            canonicalize_helper_base_url("http://helper.example:80/").unwrap(),
            "http://helper.example"
        );
    }

    #[test]
    fn preserves_non_default_ports_and_mount_paths() {
        assert_eq!(
            canonicalize_helper_base_url("https://helper.example:8443/vote").unwrap(),
            "https://helper.example:8443/vote"
        );
        assert_eq!(
            canonicalize_helper_base_url("https://helper.example/vote%20mount/").unwrap(),
            "https://helper.example/vote%20mount"
        );
    }

    #[test]
    fn rejects_non_base_or_credentialed_urls() {
        for value in [
            "file:///tmp/helper",
            "https://user@example.com",
            "https://example.com?helper=1",
            "https://example.com/#fragment",
        ] {
            assert!(canonicalize_helper_base_url(value).is_err(), "{value}");
        }
    }

    #[test]
    fn canonical_list_deduplicates_equivalent_urls_and_rejects_invalid_entries() {
        let urls = vec![
            "HTTPS://Helper.Example:443/vote/".to_string(),
            "https://helper.example/vote".to_string(),
            "https://other.example".to_string(),
        ];
        assert_eq!(
            canonical_helper_url_list(&urls).unwrap(),
            vec![
                "https://helper.example/vote".to_string(),
                "https://other.example".to_string(),
            ]
        );

        let invalid = vec![
            "https://helper.example".to_string(),
            "https://other.example?tenant=1".to_string(),
        ];
        assert!(canonical_helper_url_list(&invalid).is_err());
    }
}
