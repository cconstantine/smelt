use super::*;

fn template(s: &str) -> PreviewTemplate {
    PreviewTemplate::parse(s).expect("a valid template")
}

#[test]
fn test_the_default_template_builds_and_matches_dev_addresses() {
    let t = template(DEFAULT_PREVIEW_URL);
    assert_eq!(t.url_for(42, PodHost::Localhost, 3000), "http://3000-42.preview.localhost:8181");
    assert_eq!(t.match_host("3000-42.preview.localhost:8181"), Some((42, PodHost::Localhost, 3000)));
    assert_eq!(t.match_host("3000-42.Preview.LOCALHOST:8181"), Some((42, PodHost::Localhost, 3000)));
}

#[test]
fn test_a_production_template_under_a_wildcard_domain() {
    let t = template("https://{port}-{conversation}-smelt.constantlee.us");
    assert_eq!(t.url_for(7, PodHost::Localhost, 5173), "https://5173-7-smelt.constantlee.us");
    assert_eq!(t.match_host("5173-7-smelt.constantlee.us"), Some((7, PodHost::Localhost, 5173)));
}

#[test]
fn test_the_placeholders_can_come_in_either_order() {
    let t = template("https://c{conversation}-p{port}.example.com");
    assert_eq!(t.url_for(12, PodHost::Localhost, 8080), "https://c12-p8080.example.com");
    assert_eq!(t.match_host("c12-p8080.example.com"), Some((12, PodHost::Localhost, 8080)));
}

#[test]
fn test_a_container_preview_carries_its_address_in_the_port_slot() {
    let container = PodHost::Container(std::net::Ipv4Addr::new(172, 21, 0, 2));
    let t = template(DEFAULT_PREVIEW_URL);
    assert_eq!(t.url_for(42, container, 3000), "http://172-21-0-2-3000-42.preview.localhost:8181");
    assert_eq!(
        t.match_host("172-21-0-2-3000-42.preview.localhost:8181"),
        Some((42, container, 3000))
    );
    let t = template("https://c{conversation}-p{port}.example.com");
    assert_eq!(t.url_for(12, container, 8080), "https://c12-p172-21-0-2-8080.example.com");
    assert_eq!(t.match_host("c12-p172-21-0-2-8080.example.com"), Some((12, container, 8080)));
}

#[test]
fn test_container_preview_hosts_outside_the_pod_or_malformed_do_not_match() {
    let t = template(DEFAULT_PREVIEW_URL);
    for host in [
        // Outside the pod's Docker range.
        "172-24-0-1-3000-42.preview.localhost:8181",
        "10-43-0-1-443-42.preview.localhost:8181",
        // Not an address and a port.
        "21-0-2-3000-42.preview.localhost:8181",
        "172-21-0-2-42.preview.localhost:8181",
        "172-21-0-2-0-42.preview.localhost:8181",
        "172-21-0-256-3000-42.preview.localhost:8181",
        "172-21-0-2-3000--42.preview.localhost:8181",
    ] {
        assert_eq!(t.match_host(host), None, "{host:?} is not a preview");
    }
}

#[test]
fn test_hosts_that_are_not_previews_do_not_match() {
    let t = template("https://{port}-{conversation}-smelt.constantlee.us");
    for host in [
        "smelt.constantlee.us",
        "constantlee.us",
        "abc-42-smelt.constantlee.us",
        "3000-abc-smelt.constantlee.us",
        "3000--smelt.constantlee.us",
        "-42-smelt.constantlee.us",
        "3000-42-smelt.constantlee.us.evil.example",
        "evil.example.3000-42-smelt.constantlee.us",
        "3000-42-smelt.constantlee.us:8443",
        "70000-1-smelt.constantlee.us",
        "0-1-smelt.constantlee.us",
        "+3000-42-smelt.constantlee.us",
        "3000-99999999999999999999-smelt.constantlee.us",
        "",
    ] {
        assert_eq!(t.match_host(host), None, "{host:?} is not a preview");
    }
}

#[test]
fn test_preview_links_pair_each_port_with_its_address() {
    let t = template(DEFAULT_PREVIEW_URL);
    let container = PodHost::Container(std::net::Ipv4Addr::new(172, 21, 0, 2));
    let links = preview_links(&t, 9, &[(PodHost::Localhost, 3000), (PodHost::Localhost, 5173), (container, 3000)]);
    let link = |port, host: Option<&str>, url: &str| crate::events::SandboxPreview {
        port,
        host: host.map(str::to_string),
        url: url.to_string(),
    };
    assert_eq!(
        links,
        vec![
            link(3000, None, "http://3000-9.preview.localhost:8181"),
            link(5173, None, "http://5173-9.preview.localhost:8181"),
            link(3000, Some("172.21.0.2"), "http://172-21-0-2-3000-9.preview.localhost:8181"),
        ]
    );
}

#[test]
fn test_templates_missing_a_placeholder_scheme_or_with_a_path_are_refused() {
    for bad in [
        "https://{port}-smelt.example.com",
        "https://{conversation}-smelt.example.com",
        "{port}-{conversation}.example.com",
        "ftp://{port}-{conversation}.example.com",
        "https://{port}-{conversation}.example.com/preview",
        "https://{port}{conversation}.example.com",
        "https://{port}-{port}-{conversation}.example.com",
    ] {
        assert!(PreviewTemplate::parse(bad).is_err(), "{bad:?} should be refused");
    }
}

/// A browser leaves a scheme's default port out of `Host` and `Origin`,
/// so a template that spells it out must too (SME-183).
#[test]
fn test_a_default_port_is_dropped_as_a_browser_drops_it() {
    for (template_text, url, host) in [
        ("https://{port}-{conversation}.preview.example.com:443", "https://3000-42.preview.example.com", "3000-42.preview.example.com"),
        ("http://{port}-{conversation}.preview.example.com:80", "http://3000-42.preview.example.com", "3000-42.preview.example.com"),
        ("https://{port}-{conversation}.preview.example.com:", "https://3000-42.preview.example.com", "3000-42.preview.example.com"),
        ("https://{port}-{conversation}.preview.example.com:0443", "https://3000-42.preview.example.com", "3000-42.preview.example.com"),
        ("https://{port}-{conversation}.Preview.EXAMPLE.com:443", "https://3000-42.preview.example.com", "3000-42.PREVIEW.example.com"),
    ] {
        let t = template(template_text);
        assert_eq!(t.url_for(42, PodHost::Localhost, 3000), url, "{template_text:?}");
        assert_eq!(t.match_host(host), Some((42, PodHost::Localhost, 3000)), "{template_text:?} should match {host:?}");
    }
}

#[test]
fn test_a_non_default_port_is_kept_in_the_form_a_browser_sends() {
    for (template_text, url, host) in [
        ("https://{port}-{conversation}.example.com:8443", "https://3000-42.example.com:8443", "3000-42.example.com:8443"),
        ("https://{port}-{conversation}.example.com:80", "https://3000-42.example.com:80", "3000-42.example.com:80"),
        ("http://{port}-{conversation}.example.com:443", "http://3000-42.example.com:443", "3000-42.example.com:443"),
        ("http://{port}-{conversation}.example.com:08181", "http://3000-42.example.com:8181", "3000-42.example.com:8181"),
    ] {
        let t = template(template_text);
        assert_eq!(t.url_for(42, PodHost::Localhost, 3000), url, "{template_text:?}");
        assert_eq!(t.match_host(host), Some((42, PodHost::Localhost, 3000)), "{template_text:?} should match {host:?}");
        assert_eq!(t.match_host("3000-42.example.com"), None, "{template_text:?} needs its port");
    }
}

#[test]
fn test_a_trailing_dot_is_kept_in_links_and_matched() {
    let t = template("https://{port}-{conversation}.preview.example.com.:443");
    assert_eq!(t.url_for(42, PodHost::Localhost, 3000), "https://3000-42.preview.example.com.");
    assert_eq!(t.match_host("3000-42.preview.example.com."), Some((42, PodHost::Localhost, 3000)));
}

#[test]
fn test_a_port_placeholder_in_the_port_is_left_alone() {
    let t = template("http://{conversation}.x.test:{port}");
    assert_eq!(t.url_for(42, PodHost::Localhost, 3000), "http://42.x.test:3000");
    assert_eq!(t.match_host("42.x.test:3000"), Some((42, PodHost::Localhost, 3000)));
}

/// Templates accepted before SME-183 that no browser's `Host` can match.
#[test]
fn test_templates_no_browser_host_can_match_are_refused() {
    for bad in [
        "https://user@{port}-{conversation}.example.com",
        "http://[{port}-{conversation}]:8181",
        "http://[::1]:{port}-{conversation}",
        "https://{port}-{conversation}.bücher.example",
        "https://{port}-{conversation}.example.com:99999",
        "https://{port}-{conversation}.example.com:44a",
        "https://{port}-{conversation}.example.com:+443",
        "https://{port}-{conversation}.example.com:0",
    ] {
        let error = PreviewTemplate::parse(bad).expect_err(&format!("{bad:?} should be refused"));
        assert!(error.contains(bad), "the error should name the template: {error:?}");
    }
    let error = PreviewTemplate::parse("https://{port}-{conversation}.bücher.example").expect_err("non-ASCII");
    assert!(error.contains("xn--"), "the error should say to write the xn-- form: {error:?}");
}

/// A proxy in front of the listener may write the default port into
/// `Host` (nginx's `$host:$server_port`); `Origin` is still matched
/// exactly, as a browser writes it (SME-183's review).
#[test]
fn test_a_request_host_spelling_out_the_default_port_still_matches() {
    let t = template("https://{port}-{conversation}.preview.example.com:443");
    assert_eq!(t.match_request_host("3000-42.preview.example.com:443"), Some((42, PodHost::Localhost, 3000)));
    assert_eq!(t.match_request_host("3000-42.preview.example.com"), Some((42, PodHost::Localhost, 3000)));
    assert_eq!(t.match_request_host("3000-42.preview.example.com:80"), None);
    assert_eq!(t.match_host("3000-42.preview.example.com:443"), None, "an Origin spelling out :443 isn't a browser's");
    let t = template("http://{port}-{conversation}.preview.example.com");
    assert_eq!(t.match_request_host("3000-42.preview.example.com:80"), Some((42, PodHost::Localhost, 3000)));
    assert_eq!(t.match_request_host("3000-42.preview.example.com:443"), None);
}
