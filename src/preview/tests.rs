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
