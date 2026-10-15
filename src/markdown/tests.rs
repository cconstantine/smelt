use super::*;

fn text(s: &str) -> Inline {
    Inline::Text(s.to_string())
}

#[test]
fn test_a_paragraph_with_emphasis_strong_code_and_strike() {
    assert_eq!(
        parse("Some *soft* and **loud** `code` ~~gone~~."),
        vec![Block::Paragraph(vec![
            text("Some "),
            Inline::Emphasis(vec![text("soft")]),
            text(" and "),
            Inline::Strong(vec![text("loud")]),
            text(" "),
            Inline::Code("code".into()),
            text(" "),
            Inline::Strike(vec![text("gone")]),
            text("."),
        ])]
    );
}

#[test]
fn test_headings_keep_their_level() {
    assert_eq!(
        parse("# One\n\n### Three"),
        vec![
            Block::Heading { level: 1, content: vec![text("One")] },
            Block::Heading { level: 3, content: vec![text("Three")] },
        ]
    );
}

#[test]
fn test_a_tight_nested_list_and_a_task_list() {
    assert_eq!(
        parse("- a\n  1. b\n- [x] done\n- [ ] todo"),
        vec![Block::List {
            start: None,
            items: vec![
                ListItem {
                    task: None,
                    blocks: vec![
                        Block::Plain(vec![text("a")]),
                        Block::List {
                            start: Some(1),
                            items: vec![ListItem { task: None, blocks: vec![Block::Plain(vec![text("b")])] }],
                        },
                    ],
                },
                ListItem { task: Some(true), blocks: vec![Block::Plain(vec![text("done")])] },
                ListItem { task: Some(false), blocks: vec![Block::Plain(vec![text("todo")])] },
            ],
        }]
    );
}

#[test]
fn test_a_table() {
    assert_eq!(
        parse("| a | b |\n|---|--:|\n| 1 | **2** |"),
        vec![Block::Table {
            head: vec![vec![text("a")], vec![text("b")]],
            rows: vec![vec![vec![text("1")], vec![Inline::Strong(vec![text("2")])]]],
        }]
    );
}

#[test]
fn test_a_quote_and_a_rule() {
    assert_eq!(
        parse("> quoted\n\n---"),
        vec![Block::Quote(vec![Block::Paragraph(vec![text("quoted")])]), Block::Rule]
    );
}

#[test]
fn test_a_closed_fence_keeps_its_language_and_text() {
    assert_eq!(
        parse("```rust\nfn main() {}\n```\n"),
        vec![Block::Code { lang: Some("rust".into()), text: "fn main() {}\n".into(), closed: true }]
    );
}

#[test]
fn test_an_unclosed_fence_mid_stream_is_not_closed() {
    assert_eq!(
        parse("Here:\n\n```py\nprint(1)\n"),
        vec![
            Block::Paragraph(vec![text("Here:")]),
            Block::Code { lang: Some("py".into()), text: "print(1)\n".into(), closed: false },
        ]
    );
    // The fence line itself, before any code.
    assert_eq!(parse("```"), vec![Block::Code { lang: None, text: String::new(), closed: false }]);
}

#[test]
fn test_a_longer_closing_fence_and_a_tilde_fence_close() {
    assert_eq!(
        parse("````\na\n`````"),
        vec![Block::Code { lang: None, text: "a\n".into(), closed: true }]
    );
    assert_eq!(parse("~~~\nb\n~~~"), vec![Block::Code { lang: None, text: "b\n".into(), closed: true }]);
}

fn code_closed(source: &str) -> Vec<bool> {
    fn walk(blocks: &[Block], out: &mut Vec<bool>) {
        for block in blocks {
            match block {
                Block::Code { closed, .. } => out.push(*closed),
                Block::Quote(inner) => walk(inner, out),
                Block::List { items, .. } => items.iter().for_each(|i| walk(&i.blocks, out)),
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(&parse(source), &mut out);
    out
}

/// SME-30 code review: a fence ended by its quote or list item (with
/// more of the reply after it) is finished, and a content line that
/// only looks like a fence (indented four spaces, or behind a `>` in a
/// top-level block) doesn't close one.
#[test]
fn test_fence_closed_follows_what_pulldown_cmark_ended() {
    assert_eq!(code_closed("> ```rust\n> fn a() {}\n\nafter"), vec![true], "ended by its quote");
    assert_eq!(code_closed("- ```rust\n  fn a() {}\n\nafter"), vec![true], "ended by its list item");
    assert_eq!(code_closed("> ```rust\n> fn a() {}\n> ```"), vec![true], "closed inside a quote");
    assert_eq!(code_closed("```\ncode\n    ```"), vec![false], "an indented fence-like line is content");
    assert_eq!(code_closed("```md\n> ```"), vec![false], "a quoted fence-like line is content");
    assert_eq!(code_closed("```\n```"), vec![true], "an empty closed block");
    assert_eq!(code_closed("```"), vec![false], "just the opening fence");
    assert_eq!(code_closed("```py\nprint(1)\n``"), vec![false], "a fence still arriving");
    assert_eq!(code_closed("> ```rust\n> foo\n> \n> bar\n>"), vec![false], "a blank quoted line after content");
    assert_eq!(code_closed("> ```\n> foo\n>"), vec![false], "ends on a blank quoted line");
    assert_eq!(code_closed("> ```rust\n>"), vec![false], "an empty quoted block still open");
}

#[test]
fn test_raw_html_blocks_and_inline_html_come_out_as_text() {
    assert_eq!(
        parse("<script>alert(1)</script>\n\nhi <img src=x onerror=alert(1)> there"),
        vec![
            Block::Html("<script>alert(1)</script>\n".into()),
            Block::Paragraph(vec![text("hi <img src=x onerror=alert(1)> there")]),
        ]
    );
}

#[test]
fn test_only_http_https_and_mailto_links_are_links() {
    assert_eq!(
        parse("[a](https://e.com) [b](mailto:x@e.com) [c](javascript:alert(1)) [d](data:text/html,x) [e](/rel)"),
        vec![Block::Paragraph(vec![
            Inline::Link { url: "https://e.com".into(), content: vec![text("a")] },
            text(" "),
            Inline::Link { url: "mailto:x@e.com".into(), content: vec![text("b")] },
            text(" c d e"),
        ])]
    );
}

#[test]
fn test_only_http_and_https_images_load() {
    assert_eq!(
        parse("![cat](https://e.com/c.png) ![x](javascript:1) ![y](data:image/png;base64,AA)"),
        vec![Block::Paragraph(vec![
            Inline::Image { url: "https://e.com/c.png".into(), alt: "cat".into() },
            text(" x y"),
        ])]
    );
}

/// SME-30 code review: an image's alt text keeps its formatted words.
#[test]
fn test_an_images_alt_keeps_formatted_words() {
    assert_eq!(
        parse("![see **this** `big` *chart*](https://x/y.png)"),
        vec![Block::Paragraph(vec![Inline::Image { url: "https://x/y.png".into(), alt: "see this big chart".into() }])]
    );
    assert_eq!(parse("![see *this*\nchart](ftp://x)"), vec![Block::Paragraph(vec![text("see this chart")])]);
}

#[test]
fn test_uppercase_schemes_count_as_their_scheme() {
    assert_eq!(
        parse("[a](HTTPS://e.com) [b](JavaScript:1)"),
        vec![Block::Paragraph(vec![
            Inline::Link { url: "HTTPS://e.com".into(), content: vec![text("a")] },
            text(" b"),
        ])]
    );
}

#[test]
fn test_line_breaks() {
    assert_eq!(
        parse("a\nb  \nc"),
        vec![Block::Paragraph(vec![text("a"), Inline::SoftBreak, text("b"), Inline::HardBreak, text("c")])]
    );
}

#[test]
fn test_non_ascii_and_an_empty_reply() {
    assert_eq!(parse("héllo 你好 🎉"), vec![Block::Paragraph(vec![text("héllo 你好 🎉")])]);
    assert_eq!(parse(""), Vec::<Block>::new());
}

#[test]
fn test_footnotes_and_math_stay_text() {
    assert_eq!(parse("a[^1] $x$"), vec![Block::Paragraph(vec![text("a[^1] $x$")])]);
}

fn depth(blocks: &[Block]) -> usize {
    blocks
        .iter()
        .map(|b| match b {
            Block::Quote(inner) => 1 + depth(inner),
            Block::List { items, .. } => 1 + items.iter().map(|i| depth(&i.blocks)).max().unwrap_or(0),
            _ => 0,
        })
        .max()
        .unwrap_or(0)
}

#[test]
fn test_nesting_past_the_limit_is_flattened() {
    let quotes = format!("{}deep", "> ".repeat(100));
    let blocks = parse(&quotes);
    assert_eq!(depth(&blocks), MAX_DEPTH, "{blocks:?}");
    let lists: String = (0..100).map(|i| format!("{}- l{i}\n", "  ".repeat(i))).collect();
    assert!(depth(&parse(&lists)) <= MAX_DEPTH);
    // The innermost text survives the flattening.
    assert!(format!("{:?}", parse(&quotes)).contains("deep"));
}

fn inline_depth(inlines: &[Inline]) -> usize {
    inlines
        .iter()
        .map(|i| match i {
            Inline::Emphasis(c) | Inline::Strong(c) | Inline::Strike(c) | Inline::Link { content: c, .. } => {
                1 + inline_depth(c)
            }
            _ => 0,
        })
        .max()
        .unwrap_or(0)
}

/// SME-30 code review: emphasis, strong, strikethrough and links nest
/// too, and render recursively, so they're bounded like quotes and
/// lists.
#[test]
fn test_inline_nesting_past_the_limit_is_flattened() {
    let stars = "*".repeat(2_000);
    let blocks = parse(&format!("{stars}deep{stars}"));
    let Some(Block::Paragraph(content)) = blocks.first() else {
        panic!("a paragraph: {blocks:?}");
    };
    assert!(inline_depth(content) <= MAX_DEPTH, "inline nesting {} deep", inline_depth(content));
    assert!(format!("{content:?}").contains("deep"), "the innermost text survives");
    let links = format!("{}x{}", "[".repeat(100), "](https://e.com)".repeat(100));
    let Some(Block::Paragraph(content)) = parse(&links).first().cloned() else {
        panic!("a paragraph");
    };
    assert!(inline_depth(&content) <= MAX_DEPTH);
}

fn link(url: &str, visible: &str) -> Inline {
    Inline::Link { url: url.into(), content: vec![text(visible)] }
}

/// Every link in a parsed reply, in order: its `href` and its words.
fn links_in(source: &str) -> Vec<(String, String)> {
    fn inlines(content: &[Inline], out: &mut Vec<(String, String)>) {
        for inline in content {
            match inline {
                Inline::Link { url, content } => {
                    let mut words = String::new();
                    content.iter().for_each(|i| plain_text(i, &mut words));
                    out.push((url.clone(), words));
                }
                Inline::Emphasis(c) | Inline::Strong(c) | Inline::Strike(c) => inlines(c, out),
                _ => {}
            }
        }
    }
    fn blocks(content: &[Block], out: &mut Vec<(String, String)>) {
        for block in content {
            match block {
                Block::Paragraph(c) | Block::Plain(c) | Block::Heading { content: c, .. } => inlines(c, out),
                Block::Quote(inner) => blocks(inner, out),
                Block::List { items, .. } => items.iter().for_each(|i| blocks(&i.blocks, out)),
                Block::Table { head, rows } => {
                    head.iter().chain(rows.iter().flatten()).for_each(|cell| inlines(cell, out))
                }
                Block::Code { .. } | Block::Rule | Block::Html(_) => {}
            }
        }
    }
    let mut out = Vec::new();
    blocks(&parse(source), &mut out);
    out
}

fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected.iter().map(|(u, t)| (u.to_string(), t.to_string())).collect()
}

/// SME-104: a bare URL in a reply is a link whose words are the URL,
/// with the text around it kept.
#[test]
fn test_a_bare_url_becomes_a_link_and_keeps_the_text_around_it() {
    assert_eq!(
        parse("See https://e.com/x?y=1#z now"),
        vec![Block::Paragraph(vec![text("See "), link("https://e.com/x?y=1#z", "https://e.com/x?y=1#z"), text(" now")])]
    );
    assert_eq!(
        links_in("a http://e.com b HTTPS://E.COM/X"),
        pairs(&[("http://e.com", "http://e.com"), ("HTTPS://E.COM/X", "HTTPS://E.COM/X")])
    );
}

/// SME-104: trailing punctuation isn't part of a URL, a balanced `)`
/// is, and a quote ends one.
#[test]
fn test_a_bare_urls_trailing_punctuation_and_parentheses() {
    assert_eq!(links_in("see https://e.com."), pairs(&[("https://e.com", "https://e.com")]));
    assert_eq!(
        links_in("a https://a.com, b https://b.com! c https://c.com: d https://d.com; e https://e.com?"),
        pairs(&[
            ("https://a.com", "https://a.com"),
            ("https://b.com", "https://b.com"),
            ("https://c.com", "https://c.com"),
            ("https://d.com", "https://d.com"),
            ("https://e.com", "https://e.com"),
        ])
    );
    assert_eq!(links_in("(https://e.com)"), pairs(&[("https://e.com", "https://e.com")]));
    let wiki = "https://en.wikipedia.org/wiki/Rust_(programming_language)";
    assert_eq!(links_in(&format!("{wiki}.")), pairs(&[(wiki, wiki)]));
    let inner = "https://en.wikipedia.org/wiki/Rust_(language)";
    assert_eq!(links_in(&format!("(see {inner})")), pairs(&[(inner, inner)]));
    assert_eq!(links_in("\"https://e.com/\" said"), pairs(&[("https://e.com/", "https://e.com/")]));
}

/// SME-104: a `www.` address links to `https://`, its text unchanged;
/// other names with a dot (file names, bare domains) stay text.
#[test]
fn test_www_addresses_link_and_file_names_stay_text() {
    assert_eq!(
        parse("go to www.example.com/a."),
        vec![Block::Paragraph(vec![
            text("go to "),
            link("https://www.example.com/a", "www.example.com/a"),
            text("."),
        ])]
    );
    assert_eq!(links_in("WWW.E.COM"), pairs(&[("https://WWW.E.COM", "WWW.E.COM")]));
    assert_eq!(
        parse("See readme.md, main.rs, foo.bar() and example.com or e.g. this."),
        vec![Block::Paragraph(vec![text("See readme.md, main.rs, foo.bar() and example.com or e.g. this.")])]
    );
}

/// SME-104: an email address links to `mailto:`.
#[test]
fn test_a_bare_email_address_links_to_mailto() {
    assert_eq!(
        parse("Mail me@example.com."),
        vec![Block::Paragraph(vec![text("Mail "), link("mailto:me@example.com", "me@example.com"), text(".")])]
    );
    assert_eq!(
        links_in("or first.last+tag@mail.example.org"),
        pairs(&[("mailto:first.last+tag@mail.example.org", "first.last+tag@mail.example.org")])
    );
    assert_eq!(
        links_in("user.name@e.com"),
        pairs(&[("mailto:user.name@e.com", "user.name@e.com")]),
        "an address with a dot in its name is found whole"
    );
    assert_eq!(links_in("root@localhost"), pairs(&[]), "a domain without a dot stays text");
}

/// SME-104: `<me@e.com>` came through with no `mailto:`, so it was
/// refused as a link; an angle-bracket URL stays one link.
#[test]
fn test_angle_bracket_autolinks() {
    assert_eq!(
        parse("<me@e.com>"),
        vec![Block::Paragraph(vec![link("mailto:me@e.com", "me@e.com")])]
    );
    assert_eq!(parse("<https://e.com>"), vec![Block::Paragraph(vec![link("https://e.com", "https://e.com")])]);
}

/// SME-104: code, raw HTML blocks, image alt text and an existing
/// link's words are left alone, so a link never nests in a link.
#[test]
fn test_code_html_blocks_alt_text_and_links_are_not_autolinked() {
    assert_eq!(parse("`https://e.com`"), vec![Block::Paragraph(vec![Inline::Code("https://e.com".into())])]);
    assert_eq!(links_in("```\nhttps://e.com\n```\n\n    https://f.com\n"), pairs(&[]));
    assert_eq!(parse("<div>\nhttps://e.com\n</div>"), vec![Block::Html("<div>\nhttps://e.com\n</div>".into())]);
    assert_eq!(
        parse("![https://e.com/a](https://e.com/b.png)"),
        vec![Block::Paragraph(vec![Inline::Image { url: "https://e.com/b.png".into(), alt: "https://e.com/a".into() }])]
    );
    assert_eq!(
        parse("[https://a.com](https://b.com)"),
        vec![Block::Paragraph(vec![link("https://b.com", "https://a.com")])]
    );
    assert_eq!(
        parse("[see *www.a.com*](https://b.com)"),
        vec![Block::Paragraph(vec![Inline::Link {
            url: "https://b.com".into(),
            content: vec![text("see "), Inline::Emphasis(vec![text("www.a.com")])],
        }])]
    );
}

/// SME-104 review 1: an email address whose name holds URL syntax
/// (`?`, `#`, `%`, `&`, `/`) would send a `mailto:` somewhere its text
/// doesn't say (`billing?cc=x@evil.com` is a mail to "billing" copied to
/// x@evil.com), and bidi controls reorder what a link shows; both stay
/// text, bare or in angle brackets.
#[test]
fn test_links_that_would_not_go_where_they_say_stay_text() {
    for source in [
        "billing?cc=attacker@evil.com",
        "attacker%40evil.com?@bank.com",
        "a#b@e.com",
        "a&b@e.com",
        "a/b@e.com",
        "<billing?cc=attacker@evil.com>",
        "<https://e.com/\u{202E}moc.knab>",
        "<mailto:billing?cc=attacker@evil.com>",
        "<MAILTO:a%40evil.com@bank.com>",
        "a\u{202E}b@e.com",
        "https://e.com/\u{202E}moc.knab",
        "www.e\u{2066}x.com",
        "see \u{200F}me@e.com",
    ] {
        assert_eq!(links_in(source), pairs(&[]), "{source:?}");
    }
    assert_eq!(
        links_in("mail first.last+tag@e.com"),
        pairs(&[("mailto:first.last+tag@e.com", "first.last+tag@e.com")]),
        "an ordinary address still links"
    );
    assert_eq!(
        links_in("<mailto:me@e.com>"),
        pairs(&[("mailto:me@e.com", "mailto:me@e.com")]),
        "an ordinary mailto: autolink still links"
    );
}

/// SME-104 review 1: a refused link or image is plain text before
/// `autolink` runs, so a URL in its words links to where they say.
#[test]
fn test_a_refused_links_or_images_words_are_autolinked_like_text() {
    assert_eq!(
        parse("[https://a.com](javascript:x) ![www.b.com](ftp://c/d.png)"),
        vec![Block::Paragraph(vec![
            link("https://a.com", "https://a.com"),
            text(" "),
            link("https://www.b.com", "www.b.com"),
        ])]
    );
}

/// SME-104: only `http` and `https` URLs link; other schemes stay text.
#[test]
fn test_other_schemes_stay_text() {
    assert_eq!(
        parse("ftp://e.com file:///etc/passwd javascript://alert(1) data://x.com/y"),
        vec![Block::Paragraph(vec![text("ftp://e.com file:///etc/passwd javascript://alert(1) data://x.com/y")])]
    );
}

/// SME-104: pulldown-cmark splits a URL at an escape or an entity; the
/// link covers the whole URL, unescaped.
#[test]
fn test_a_url_split_by_escapes_and_entities_is_one_link() {
    assert_eq!(
        links_in("see https://e.com/a\\_b?x=1&amp;y=2 now"),
        pairs(&[("https://e.com/a_b?x=1&y=2", "https://e.com/a_b?x=1&y=2")])
    );
}

/// SME-104: bare URLs link inside formatting and every block that
/// holds text.
#[test]
fn test_bare_urls_link_inside_formatting_and_blocks() {
    let source = "*https://a.com* **https://b.com** ~~https://c.com~~\n\n\
            # https://d.com\n\n| h |\n|---|\n| https://e.com |\n\n- https://f.com\n\n> https://g.com\n";
    let found: Vec<String> = links_in(source).into_iter().map(|(u, _)| u).collect();
    assert_eq!(
        found,
        ["https://a.com", "https://b.com", "https://c.com", "https://d.com", "https://e.com", "https://f.com", "https://g.com"]
    );
}

/// SME-104: several URLs in one run of text, with non-ASCII around
/// them.
#[test]
fn test_several_urls_and_non_ascii_text() {
    assert_eq!(
        parse("héllo https://a.com 你好 www.b.com/ü 🎉 c@d.io"),
        vec![Block::Paragraph(vec![
            text("héllo "),
            link("https://a.com", "https://a.com"),
            text(" 你好 "),
            link("https://www.b.com/ü", "www.b.com/ü"),
            text(" 🎉 "),
            link("mailto:c@d.io", "c@d.io"),
        ])]
    );
}

/// SME-104: what a URL looks like while the reply streaming it is
/// still arriving.
#[test]
fn test_half_written_urls_while_streaming() {
    assert_eq!(links_in("see https://"), pairs(&[]));
    assert_eq!(links_in("see www."), pairs(&[]));
    assert_eq!(links_in("mail me@"), pairs(&[]));
    assert_eq!(links_in("mail me@ex"), pairs(&[]));
    assert_eq!(links_in("see https://exa"), pairs(&[("https://exa", "https://exa")]));
    assert_eq!(links_in("[docs](https://ex"), pairs(&[("https://ex", "https://ex")]));
    assert_eq!(links_in("[docs](https://ex.com)"), pairs(&[("https://ex.com", "docs")]));
}

/// SME-104: a URL nested as deep as formatting goes stays text rather
/// than making the tree deeper than `MAX_DEPTH`.
#[test]
fn test_autolinking_keeps_inline_nesting_within_the_limit() {
    let stars = "*".repeat(2_000);
    let blocks = parse(&format!("{stars}https://e.com{stars}"));
    let Some(Block::Paragraph(content)) = blocks.first() else {
        panic!("a paragraph: {blocks:?}");
    };
    assert!(inline_depth(content) <= MAX_DEPTH, "inline nesting {} deep", inline_depth(content));
    assert!(format!("{content:?}").contains("https://e.com"), "the URL's text survives");
    // One level shallower, there's room for the link.
    let fifteen = (0..MAX_DEPTH - 1).fold("https://e.com".to_string(), |inner, _| format!("*x {inner} y*"));
    let Some(Block::Paragraph(content)) = parse(&fifteen).first().cloned() else {
        panic!("a paragraph");
    };
    assert_eq!(inline_depth(&content), MAX_DEPTH, "fifteen emphases and the link: {content:?}");
    assert_eq!(links_in(&fifteen), pairs(&[("https://e.com", "https://e.com")]));
}

#[test]
fn test_very_deep_nesting_and_long_input_parse() {
    let deep = format!("{}x", "> ".repeat(500));
    assert!(!parse(&deep).is_empty());
    let long = "word ".repeat(20_000);
    assert_eq!(parse(&long).len(), 1);
}
