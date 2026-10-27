//! Where to support the project. The menu's "Support Agent Terminal…" entry hands one of these
//! fixed URLs to the browser on a user action; the app makes no request to them itself.

/// The project's GitHub Sponsors page. `None` hides its entry in the UI.
pub const GITHUB_SPONSORS: Option<&str> = None;

/// The Ko-fi page. `None` hides its entry in the UI.
pub const KO_FI: Option<&str> = Some("https://ko-fi.com/jean1880");

/// The (label, url) pairs the UI offers, each only when it is set.
pub fn links() -> Vec<(&'static str, &'static str)> {
    links_with(GITHUB_SPONSORS, KO_FI)
}

/// [`links`] for given values, so the rule is testable without editing the consts.
fn links_with(
    sponsors: Option<&'static str>,
    ko_fi: Option<&'static str>,
) -> Vec<(&'static str, &'static str)> {
    let mut links = Vec::new();
    if let Some(url) = sponsors {
        links.push(("GitHub Sponsors", url));
    }
    if let Some(url) = ko_fi {
        links.push(("Support on Ko-fi", url));
    }
    links
}

/// The chooser names its responses `link-N`, N being the index into `links`. The URL a response
/// stands for, or `None` for the dismiss response, a malformed id or an index out of range.
pub fn url_for_response(
    links: &[(&'static str, &'static str)],
    response: &str,
) -> Option<&'static str> {
    let index = response.strip_prefix("link-")?.parse::<usize>().ok()?;
    links.get(index).map(|(_, url)| *url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_links_are_https_and_never_empty() {
        let links = links();
        assert!(!links.is_empty(), "the Support menu needs a link");
        if let Some(url) = GITHUB_SPONSORS {
            assert!(url.starts_with("https://github.com/sponsors/"));
        }
        if let Some(url) = KO_FI {
            assert!(url.starts_with("https://ko-fi.com/"));
        }
    }

    #[test]
    fn an_unset_link_hides_its_entry() {
        let sponsors = "https://github.com/sponsors/someone";
        let ko_fi = "https://ko-fi.com/someone";
        assert_eq!(
            links_with(None, Some(ko_fi)),
            vec![("Support on Ko-fi", ko_fi)]
        );
        assert_eq!(
            links_with(Some(sponsors), None),
            vec![("GitHub Sponsors", sponsors)]
        );
        let both = links_with(Some(sponsors), Some(ko_fi));
        assert_eq!(both.len(), 2);
        assert_eq!(both[1].0, "Support on Ko-fi");
    }

    #[test]
    fn a_chooser_response_maps_to_its_link() {
        let links = links_with(
            Some("https://github.com/sponsors/someone"),
            Some("https://ko-fi.com/someone"),
        );
        assert_eq!(
            url_for_response(&links, "link-1"),
            Some("https://ko-fi.com/someone")
        );
        assert_eq!(url_for_response(&links, "link-2"), None, "out of range");
        assert_eq!(url_for_response(&links, "link-x"), None, "malformed index");
        assert_eq!(
            url_for_response(&links, "close"),
            None,
            "not a link response"
        );
    }
}
