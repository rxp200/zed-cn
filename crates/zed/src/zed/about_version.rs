pub fn custom_version<'a>(version: &str, release_tag: Option<&'a str>) -> Option<&'a str> {
    let custom_version = release_tag?.strip_prefix("zed-cn-v")?;
    let (base_version, revision) = custom_version.rsplit_once("-r")?;
    if base_version != version
        || revision.is_empty()
        || revision.starts_with('0')
        || !revision.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    Some(custom_version)
}

#[cfg(test)]
mod tests {
    use super::custom_version;

    #[test]
    fn displays_release_revision_not_build_number() {
        assert_eq!(
            custom_version("1.18.1", Some("zed-cn-v1.18.1-r5")),
            Some("1.18.1-r5")
        );
        assert_eq!(
            custom_version("1.18.1", Some("zed-cn-v1.18.1-r123")),
            Some("1.18.1-r123")
        );
    }

    #[test]
    fn does_not_invent_a_revision_for_unmarked_builds() {
        assert_eq!(custom_version("1.18.1", None), None);
        assert_eq!(custom_version("1.18.1", Some("")), None);
    }

    #[test]
    fn rejects_mismatched_or_malformed_tags() {
        for tag in [
            "zed-cn-v1.18.0-r5",
            "v1.18.1",
            "zed-cn-v1.18.1-r",
            "zed-cn-v1.18.1-r0",
            "zed-cn-v1.18.1-r05",
            "zed-cn-v1.18.1-r5\n",
            "zed-cn-v1.18.1-r5-extra",
            "zed-cn-v1.18.1-r五",
        ] {
            assert_eq!(custom_version("1.18.1", Some(tag)), None, "{tag}");
        }
    }
}
