//! The model's replies as markdown (SME-30): a pure parse step into
//! smelt's own node tree, and the `Markdown` component that renders that
//! tree as Dioxus elements.
//!
//! The reply is untrusted text (the model may echo a web page it read),
//! so nothing here builds an HTML string or sets `dangerous_inner_html`:
//! raw HTML in a reply comes out as literal text, a link (written as one,
//! or a bare URL or email address `autolink` finds) only becomes an
//! `<a>` for `http`, `https` and `mailto`, and an image only loads for
//! `http`/`https`. See docs/frontend.md, "Markdown in replies", for what
//! this boundary covers and what it doesn't.

use dioxus::prelude::*;
use linkify::{LinkFinder, LinkKind};
use pulldown_cmark::{CodeBlockKind, Event, LinkType, Options, Parser, Tag, TagEnd};

use crate::highlight::{self, Span};

/// A block in a reply.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Paragraph(Vec<Inline>),
    /// A tight list item's text, which markdown doesn't wrap in a paragraph.
    Plain(Vec<Inline>),
    Heading { level: u8, content: Vec<Inline> },
    Quote(Vec<Block>),
    /// `start` is the first number of an ordered list, `None` for a
    /// bulleted one.
    List { start: Option<u64>, items: Vec<ListItem> },
    /// `closed` is false for a fenced block whose closing fence hasn't
    /// arrived yet (a reply still streaming), which renders plain.
    Code { lang: Option<String>, text: String, closed: bool },
    Table { head: Vec<Vec<Inline>>, rows: Vec<Vec<Vec<Inline>>> },
    Rule,
    /// Raw HTML from the reply, shown as text.
    Html(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ListItem {
    /// `Some(checked)` for a task list item.
    pub task: Option<bool>,
    pub blocks: Vec<Block>,
}

/// Inline content.
#[derive(Debug, Clone, PartialEq)]
pub enum Inline {
    Text(String),
    Code(String),
    Emphasis(Vec<Inline>),
    Strong(Vec<Inline>),
    Strike(Vec<Inline>),
    /// Only for `http`, `https` and `mailto` destinations; any other link
    /// is replaced by its content.
    Link { url: String, content: Vec<Inline> },
    /// Only for `http`/`https` sources; any other image is replaced by its
    /// alt text.
    Image { url: String, alt: String },
    SoftBreak,
    HardBreak,
}

/// How deep quotes and lists nest, and separately emphasis, strong,
/// strikethrough and links, before deeper ones are flattened into their
/// parent, so a hostile reply can't make rendering recurse without bound.
pub const MAX_DEPTH: usize = 16;

/// Parses a reply into blocks. Adjacent text is merged into one
/// `Inline::Text`.
pub fn parse(source: &str) -> Vec<Block> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_GFM);
    let mut builder = Builder { stack: vec![Container::Root(Vec::new())], depth: 0, inline_depth: 0 };
    let end = source.trim_end().len();
    for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
        let at_end = range.end >= end;
        builder.event(event, source.get(range).unwrap_or(""), at_end);
    }
    let mut blocks = builder.finish();
    autolink(&mut blocks);
    blocks
}

/// Turns bare URLs, `www.` addresses and email addresses in a reply's text
/// into links (SME-104), which pulldown-cmark doesn't do. It runs on the
/// finished tree rather than per text event, since pulldown-cmark splits
/// one URL into several events at escapes and entities.
///
/// A link's words are always the text it was found in, so it goes where it
/// says: `http`/`https` URLs link to themselves, `www.` addresses to
/// `https://` plus the text, email addresses to `mailto:` plus the text.
/// Other schemes, dotted names without `www.` (`main.rs`, `example.com`),
/// and a link that wouldn't go where its text says (see `mailto` and
/// `has_bidi_control`) stay text. Code, raw HTML blocks, a shown image's
/// alt text and an allowed link's words are left alone, so a link never
/// nests in a link; a refused link or image is plain text by now, so a URL
/// in its words is linked like any other.
fn autolink(blocks: &mut [Block]) {
    let mut www = LinkFinder::new();
    www.kinds(&[LinkKind::Url]).url_must_have_scheme(false);
    autolink_blocks(blocks, &Finders { links: LinkFinder::new(), www });
}

/// `links` finds URLs with a scheme and email addresses. `www` finds URLs
/// without a scheme, and runs only on the text between what `links`
/// found: in one pass, the `.` in `first.last@e.com` starts a schemeless
/// match (`first.last`), and the address found after it is the wrong one
/// (`last@e.com`'s tail).
struct Finders {
    links: LinkFinder,
    www: LinkFinder,
}

fn autolink_blocks(blocks: &mut [Block], finder: &Finders) {
    for block in blocks {
        match block {
            Block::Paragraph(content) | Block::Plain(content) | Block::Heading { content, .. } => {
                autolink_inlines(content, finder, 0)
            }
            Block::Quote(inner) => autolink_blocks(inner, finder),
            Block::List { items, .. } => {
                for item in items {
                    autolink_blocks(&mut item.blocks, finder);
                }
            }
            Block::Table { head, rows } => {
                for cell in head.iter_mut().chain(rows.iter_mut().flatten()) {
                    autolink_inlines(cell, finder, 0);
                }
            }
            Block::Code { .. } | Block::Rule | Block::Html(_) => {}
        }
    }
}

/// `depth` is how many emphasis, strong and strikethrough spans enclose
/// `inlines`: text already `MAX_DEPTH` deep stays text, so a link doesn't
/// take the tree past the limit.
fn autolink_inlines(inlines: &mut Vec<Inline>, finder: &Finders, depth: usize) {
    let mut out = Vec::with_capacity(inlines.len());
    for mut inline in std::mem::take(inlines) {
        match &mut inline {
            Inline::Text(text) if depth < MAX_DEPTH => {
                split_links(text, finder, &mut out);
                continue;
            }
            Inline::Emphasis(content) | Inline::Strong(content) | Inline::Strike(content) => {
                autolink_inlines(content, finder, depth + 1)
            }
            _ => {}
        }
        out.push(inline);
    }
    *inlines = out;
}

/// Splits `text` into text and the links found in it.
fn split_links(text: &str, finder: &Finders, out: &mut Vec<Inline>) {
    for span in finder.links.spans(text) {
        let found = span.as_str();
        match span.kind() {
            Some(LinkKind::Url) if allowed_url(found, &["http", "https"]) && !has_bidi_control(found) => {
                push_link(out, found.to_string(), found)
            }
            Some(LinkKind::Email) if let Some(url) = mailto(found) => push_link(out, url, found),
            // A URL with another scheme, or one that wouldn't go where it
            // says, stays text, all of it.
            Some(_) => push_inline(out, Inline::Text(found.to_string())),
            None => {
                for gap in finder.www.spans(found) {
                    let words = gap.as_str();
                    if gap.kind().is_some()
                        && words.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("www."))
                        && !has_bidi_control(words)
                    {
                        push_link(out, format!("https://{words}"), words);
                    } else {
                        push_inline(out, Inline::Text(words.to_string()));
                    }
                }
            }
        }
    }
}

/// The `mailto:` link for an email address, or `None` when a mail client
/// would send it somewhere its text doesn't say: URL syntax in it
/// (`billing?cc=x@evil.com` is a mail to "billing" copied to x@evil.com,
/// `x%40evil.com?@bank.com` one to x@evil.com), or bidi controls that
/// reorder how it shows. RFC 5322 allows `?`, `#`, `%`, `&` and `/` in an
/// address's name, and linkify finds them; real addresses rarely use them.
fn mailto(address: &str) -> Option<String> {
    let url_syntax = address.contains(['?', '#', '%', '&', '/']);
    (!url_syntax && !has_bidi_control(address)).then(|| format!("mailto:{address}"))
}

/// Whether `text` holds a bidi control, which reorders how the text
/// around it shows: a link holding one can show one address and go to
/// another.
fn has_bidi_control(text: &str) -> bool {
    text.chars().any(|c| matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'))
}

fn push_link(out: &mut Vec<Inline>, url: String, words: &str) {
    out.push(Inline::Link { url, content: vec![Inline::Text(words.to_string())] });
}

/// Whether a link or image destination is one smelt will follow.
fn allowed_url(url: &str, schemes: &[&str]) -> bool {
    url.split_once(':')
        .is_some_and(|(scheme, _)| schemes.iter().any(|s| scheme.eq_ignore_ascii_case(s)))
}

/// Whether a fenced code block is finished, from what pulldown-cmark made
/// of it: `raw` is its source (opening fence to wherever it ended), `text`
/// its content, and `at_end` whether it runs to the end of the reply.
///
/// A block with more of the reply after it is finished, whether a closing
/// fence or the end of its quote or list item ended it. One at the end is
/// still open when its last source line is its last content line (behind
/// nothing but quote markers and indentation); otherwise that last line
/// is the closing fence.
fn fence_closed(raw: &str, text: &str, at_end: bool) -> bool {
    if !at_end {
        return true;
    }
    // A line that's only quote markers (`>`) is blank too.
    let raw_lines: Vec<&str> = raw
        .lines()
        .filter(|l| !l.trim_start_matches(['>', ' ', '\t']).trim().is_empty())
        .collect();
    if raw_lines.len() < 2 {
        // Only the opening fence so far.
        return false;
    }
    let Some(last_raw) = raw_lines.last().map(|l| l.trim_end()) else {
        return false;
    };
    let Some(last_text) = text.lines().rev().map(str::trim_end).find(|l| !l.trim().is_empty()) else {
        // No content, and a line after the opening fence: the closing one.
        return true;
    };
    let is_content = last_raw
        .strip_suffix(last_text)
        .is_some_and(|prefix| prefix.chars().all(|c| c == '>' || c == ' ' || c == '\t'));
    !is_content
}

enum Container {
    Root(Vec<Block>),
    Quote(Vec<Block>),
    List { start: Option<u64>, items: Vec<ListItem> },
    Item { task: Option<bool>, blocks: Vec<Block> },
    /// A tight list item's inline text.
    Plain(Vec<Inline>),
    Paragraph(Vec<Inline>),
    Heading(u8, Vec<Inline>),
    /// `raw` is the block's source, `at_end` whether it runs to the end of
    /// the reply: `fence_closed` decides `closed` from them once the block
    /// ends.
    Code { lang: Option<String>, text: String, fenced: bool, raw: String, at_end: bool },
    Html(String),
    Table { head: Vec<Vec<Inline>>, rows: Vec<Vec<Vec<Inline>>> },
    Head(Vec<Vec<Inline>>),
    Row(Vec<Vec<Inline>>),
    Cell(Vec<Inline>),
    Emphasis(Vec<Inline>),
    Strong(Vec<Inline>),
    Strike(Vec<Inline>),
    /// `url` is `None` for a destination smelt won't follow.
    Link { url: Option<String>, content: Vec<Inline> },
    Image { url: Option<String>, alt: String },
    /// A tag smelt doesn't render (or a quote or list past `MAX_DEPTH`):
    /// its content goes to the container below it.
    Transparent { block: bool },
}

struct Builder {
    stack: Vec<Container>,
    /// How many quotes and lists are open.
    depth: usize,
    /// How many emphasis, strong, strikethrough and link spans are open.
    inline_depth: usize,
}

/// An inline's words, without formatting.
fn plain_text(inline: &Inline, out: &mut String) {
    match inline {
        Inline::Text(t) | Inline::Code(t) => out.push_str(t),
        Inline::Emphasis(c) | Inline::Strong(c) | Inline::Strike(c) | Inline::Link { content: c, .. } => {
            for inline in c {
                plain_text(inline, out);
            }
        }
        Inline::Image { alt, .. } => out.push_str(alt),
        Inline::SoftBreak | Inline::HardBreak => out.push(' '),
    }
}

fn push_inline(target: &mut Vec<Inline>, inline: Inline) {
    if let Inline::Text(new) = &inline
        && let Some(Inline::Text(last)) = target.last_mut()
    {
        last.push_str(new);
        return;
    }
    target.push(inline);
}

impl Builder {
    fn event(&mut self, event: Event, raw: &str, at_end: bool) {
        match event {
            Event::Start(tag) => self.start(tag, raw, at_end),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => self.text(&t),
            Event::Code(t) => self.inline(Inline::Code(t.to_string())),
            Event::Html(t) => match self.stack.last_mut() {
                Some(Container::Html(html)) => html.push_str(&t),
                _ => self.text(&t),
            },
            Event::InlineHtml(t) | Event::InlineMath(t) | Event::DisplayMath(t) | Event::FootnoteReference(t) => {
                self.text(&t)
            }
            Event::SoftBreak => self.inline(Inline::SoftBreak),
            Event::HardBreak => self.inline(Inline::HardBreak),
            Event::Rule => {
                self.close_plain();
                self.block(Block::Rule);
            }
            Event::TaskListMarker(checked) => {
                for container in self.stack.iter_mut().rev() {
                    if let Container::Item { task, .. } = container {
                        *task = Some(checked);
                        break;
                    }
                }
            }
        }
    }

    fn start(&mut self, tag: Tag, raw: &str, at_end: bool) {
        let container = match tag {
            Tag::Paragraph => Container::Paragraph(Vec::new()),
            Tag::Heading { level, .. } => Container::Heading(level as u8, Vec::new()),
            Tag::BlockQuote(_) => {
                self.depth += 1;
                if self.depth > MAX_DEPTH { Container::Transparent { block: true } } else { Container::Quote(Vec::new()) }
            }
            Tag::CodeBlock(CodeBlockKind::Fenced(info)) => Container::Code {
                lang: info.split_whitespace().next().map(str::to_string),
                text: String::new(),
                fenced: true,
                raw: raw.to_string(),
                at_end,
            },
            Tag::CodeBlock(CodeBlockKind::Indented) => {
                Container::Code { lang: None, text: String::new(), fenced: false, raw: String::new(), at_end }
            }
            Tag::HtmlBlock => Container::Html(String::new()),
            Tag::List(start) => {
                self.depth += 1;
                if self.depth > MAX_DEPTH { Container::Transparent { block: true } } else { Container::List { start, items: Vec::new() } }
            }
            Tag::Item => {
                if matches!(self.stack.last(), Some(Container::List { .. })) {
                    Container::Item { task: None, blocks: Vec::new() }
                } else {
                    Container::Transparent { block: true }
                }
            }
            Tag::Table(_) => Container::Table { head: Vec::new(), rows: Vec::new() },
            Tag::TableHead => Container::Head(Vec::new()),
            Tag::TableRow => Container::Row(Vec::new()),
            Tag::TableCell => Container::Cell(Vec::new()),
            Tag::Emphasis | Tag::Strong | Tag::Strikethrough | Tag::Link { .. } => {
                self.inline_depth += 1;
                if self.inline_depth > MAX_DEPTH {
                    Container::Transparent { block: false }
                } else {
                    match tag {
                        Tag::Emphasis => Container::Emphasis(Vec::new()),
                        Tag::Strong => Container::Strong(Vec::new()),
                        Tag::Strikethrough => Container::Strike(Vec::new()),
                        Tag::Link { link_type, dest_url, .. } => {
                            // pulldown-cmark gives `<me@e.com>` no `mailto:`.
                            // An angle-bracket link shows its own address, so
                            // it is refused where that address would mislead,
                            // as a bare one is (`split_links`).
                            let dest = match link_type {
                                LinkType::Email => mailto(&dest_url),
                                LinkType::Autolink if has_bidi_control(&dest_url) => None,
                                LinkType::Autolink
                                    if dest_url.get(..7).is_some_and(|s| s.eq_ignore_ascii_case("mailto:")) =>
                                {
                                    dest_url.get(7..).and_then(mailto)
                                }
                                _ => Some(dest_url.to_string()),
                            };
                            Container::Link {
                                url: dest.filter(|dest| allowed_url(dest, &["http", "https", "mailto"])),
                                content: Vec::new(),
                            }
                        }
                        _ => Container::Transparent { block: false },
                    }
                }
            }
            Tag::Image { dest_url, .. } => Container::Image {
                url: allowed_url(&dest_url, &["http", "https"]).then(|| dest_url.to_string()),
                alt: String::new(),
            },
            Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::MetadataBlock(_) => Container::Transparent { block: true },
            Tag::Superscript | Tag::Subscript => Container::Transparent { block: false },
        };
        let is_block = !matches!(
            container,
            Container::Emphasis(_)
                | Container::Strong(_)
                | Container::Strike(_)
                | Container::Link { .. }
                | Container::Image { .. }
                | Container::Transparent { block: false }
                | Container::Cell(_)
                | Container::Row(_)
                | Container::Head(_)
        );
        if is_block {
            self.close_plain();
        }
        self.stack.push(container);
    }

    fn end(&mut self, tag: TagEnd) {
        if matches!(tag, TagEnd::Item | TagEnd::BlockQuote(_) | TagEnd::List(_)) {
            self.close_plain();
        }
        if matches!(tag, TagEnd::BlockQuote(_) | TagEnd::List(_)) {
            self.depth = self.depth.saturating_sub(1);
        }
        if matches!(tag, TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link) {
            self.inline_depth = self.inline_depth.saturating_sub(1);
        }
        // The root is never popped: an unbalanced end is ignored.
        if self.stack.len() < 2 {
            return;
        }
        let Some(container) = self.stack.pop() else {
            return;
        };
        match container {
            Container::Root(blocks) => self.stack.push(Container::Root(blocks)),
            Container::Quote(blocks) => self.block(Block::Quote(blocks)),
            Container::List { start, items } => self.block(Block::List { start, items }),
            Container::Item { task, blocks } => {
                if let Some(Container::List { items, .. }) = self.stack.last_mut() {
                    items.push(ListItem { task, blocks });
                } else {
                    for block in blocks {
                        self.block(block);
                    }
                }
            }
            Container::Plain(content) => self.block(Block::Plain(content)),
            Container::Paragraph(content) => self.block(Block::Paragraph(content)),
            Container::Heading(level, content) => self.block(Block::Heading { level, content }),
            Container::Code { lang, text, fenced, raw, at_end } => {
                let closed = !fenced || fence_closed(&raw, &text, at_end);
                self.block(Block::Code { lang, text, closed })
            }
            Container::Html(html) => self.block(Block::Html(html)),
            Container::Table { head, rows } => self.block(Block::Table { head, rows }),
            Container::Head(cells) => {
                if let Some(Container::Table { head, .. }) = self.stack.last_mut() {
                    *head = cells;
                }
            }
            Container::Row(cells) => {
                if let Some(Container::Table { rows, .. }) = self.stack.last_mut() {
                    rows.push(cells);
                }
            }
            Container::Cell(content) => match self.stack.last_mut() {
                Some(Container::Head(cells) | Container::Row(cells)) => cells.push(content),
                _ => {
                    for inline in content {
                        self.inline(inline);
                    }
                }
            },
            Container::Emphasis(content) => self.inline(Inline::Emphasis(content)),
            Container::Strong(content) => self.inline(Inline::Strong(content)),
            Container::Strike(content) => self.inline(Inline::Strike(content)),
            Container::Link { url: Some(url), content } => self.inline(Inline::Link { url, content }),
            Container::Link { url: None, content } => {
                for inline in content {
                    self.inline(inline);
                }
            }
            Container::Image { url: Some(url), alt } => self.inline(Inline::Image { url, alt }),
            Container::Image { url: None, alt } => self.text(&alt),
            Container::Transparent { .. } => {}
        }
    }

    fn text(&mut self, text: &str) {
        match self.stack.last_mut() {
            Some(Container::Code { text: code, .. }) => code.push_str(text),
            Some(Container::Html(html)) => html.push_str(text),
            Some(Container::Image { alt, .. }) => alt.push_str(text),
            _ => self.inline(Inline::Text(text.to_string())),
        }
    }

    fn inline(&mut self, inline: Inline) {
        // An image's alt text is plain text: formatting inside it keeps
        // its words, and a line break becomes a space.
        if let Some(Container::Image { alt, .. }) = self.stack.last_mut() {
            plain_text(&inline, alt);
            return;
        }
        let needs_plain = self.needs_plain();
        let target = self.stack.iter_mut().rev().find_map(|c| match c {
            Container::Plain(v)
            | Container::Paragraph(v)
            | Container::Heading(_, v)
            | Container::Cell(v)
            | Container::Emphasis(v)
            | Container::Strong(v)
            | Container::Strike(v)
            | Container::Link { content: v, .. } => Some(v),
            _ => None,
        });
        if let Some(target) = target
            && !needs_plain
        {
            push_inline(target, inline);
            return;
        }
        // Inline content straight inside a list item (a tight list) or a
        // block container: give it a `Plain` of its own.
        self.stack.push(Container::Plain(Vec::new()));
        if let Some(Container::Plain(v)) = self.stack.last_mut() {
            push_inline(v, inline);
        }
    }

    /// Whether the innermost container that holds inlines is below a
    /// block container (so the inline needs a `Plain` of its own).
    fn needs_plain(&self) -> bool {
        for container in self.stack.iter().rev() {
            match container {
                Container::Plain(_)
                | Container::Paragraph(_)
                | Container::Heading(..)
                | Container::Cell(_)
                | Container::Emphasis(_)
                | Container::Strong(_)
                | Container::Strike(_)
                | Container::Link { .. } => return false,
                Container::Transparent { block: false } => continue,
                _ => return true,
            }
        }
        true
    }

    fn close_plain(&mut self) {
        if matches!(self.stack.last(), Some(Container::Plain(_)))
            && let Some(Container::Plain(content)) = self.stack.pop()
        {
            self.block(Block::Plain(content));
        }
    }

    /// Adds a block to the innermost container that holds blocks.
    fn block(&mut self, block: Block) {
        for container in self.stack.iter_mut().rev() {
            match container {
                Container::Root(blocks) | Container::Quote(blocks) | Container::Item { blocks, .. } => {
                    blocks.push(block);
                    return;
                }
                _ => {}
            }
        }
    }

    fn finish(mut self) -> Vec<Block> {
        // Close whatever the parser left open (it shouldn't).
        while self.stack.len() > 1 {
            self.close_plain();
            if self.stack.len() > 1 {
                self.end(TagEnd::Paragraph);
            }
        }
        match self.stack.pop() {
            Some(Container::Root(blocks)) => blocks,
            _ => Vec::new(),
        }
    }
}

/// A reply rendered from markdown. Saved replies and the streaming one
/// both use this, so a reply looks the same once it's saved.
#[component]
pub fn Markdown(
    source: String,
    /// Called when an image in the reply finishes loading (it has grown
    /// the reply after it rendered).
    #[props(default)]
    on_media_load: Option<EventHandler<()>>,
) -> Element {
    let blocks = parse(&source);
    let media = on_media_load;
    rsx! {
        div { class: "markdown", {blocks.iter().enumerate().map(|(i, block)| render_block(i, block, media))} }
    }
}

/// `media` is called when an image finishes loading.
type Media = Option<EventHandler<()>>;

fn render_blocks(blocks: &[Block], media: Media) -> Element {
    rsx! { {blocks.iter().enumerate().map(|(i, block)| render_block(i, block, media))} }
}

fn render_inlines(inlines: &[Inline], media: Media) -> Element {
    rsx! { {inlines.iter().enumerate().map(|(i, inline)| render_inline(i, inline, media))} }
}

fn render_block(key: usize, block: &Block, media: Media) -> Element {
    match block {
        Block::Paragraph(content) => rsx! { p { key: "{key}", {render_inlines(content, media)} } },
        Block::Plain(content) => rsx! { span { key: "{key}", class: "md-plain", {render_inlines(content, media)} } },
        Block::Heading { level, content } => match level {
            1 => rsx! { h1 { key: "{key}", class: "md-heading", {render_inlines(content, media)} } },
            2 => rsx! { h2 { key: "{key}", class: "md-heading", {render_inlines(content, media)} } },
            3 => rsx! { h3 { key: "{key}", class: "md-heading", {render_inlines(content, media)} } },
            4 => rsx! { h4 { key: "{key}", class: "md-heading", {render_inlines(content, media)} } },
            5 => rsx! { h5 { key: "{key}", class: "md-heading", {render_inlines(content, media)} } },
            _ => rsx! { h6 { key: "{key}", class: "md-heading", {render_inlines(content, media)} } },
        },
        Block::Quote(blocks) => rsx! { blockquote { key: "{key}", {render_blocks(blocks, media)} } },
        Block::List { start: None, items } => rsx! {
            ul { key: "{key}", {items.iter().enumerate().map(|(i, item)| render_item(i, item, media))} }
        },
        Block::List { start: Some(start), items } => {
            let start = isize::try_from(*start).unwrap_or(isize::MAX);
            rsx! {
                ol { key: "{key}", start, {items.iter().enumerate().map(|(i, item)| render_item(i, item, media))} }
            }
        }
        Block::Code { lang, text, closed } => rsx! {
            CodeBlock { key: "{key}", lang: lang.clone(), text: text.clone(), closed: *closed }
        },
        Block::Table { head, rows } => rsx! {
            div { key: "{key}", class: "md-table-wrap",
                table {
                    thead {
                        tr { {head.iter().enumerate().map(|(i, cell)| rsx! { th { key: "{i}", {render_inlines(cell, media)} } })} }
                    }
                    tbody {
                        {rows.iter().enumerate().map(|(r, row)| rsx! {
                            tr { key: "{r}",
                                {row.iter().enumerate().map(|(i, cell)| rsx! { td { key: "{i}", {render_inlines(cell, media)} } })}
                            }
                        })}
                    }
                }
            }
        },
        Block::Rule => rsx! { hr { key: "{key}" } },
        Block::Html(html) => rsx! { p { key: "{key}", class: "md-html", "{html}" } },
    }
}

fn render_item(key: usize, item: &ListItem, media: Media) -> Element {
    rsx! {
        li { key: "{key}", class: if item.task.is_some() { "md-task" },
            if let Some(checked) = item.task {
                input { r#type: "checkbox", checked, disabled: true }
            }
            {render_blocks(&item.blocks, media)}
        }
    }
}

fn render_inline(key: usize, inline: &Inline, media: Media) -> Element {
    match inline {
        Inline::Text(text) => rsx! { span { key: "{key}", "{text}" } },
        Inline::Code(code) => rsx! { code { key: "{key}", class: "md-inline-code", "{code}" } },
        Inline::Emphasis(content) => rsx! { em { key: "{key}", {render_inlines(content, media)} } },
        Inline::Strong(content) => rsx! { strong { key: "{key}", {render_inlines(content, media)} } },
        Inline::Strike(content) => rsx! { del { key: "{key}", {render_inlines(content, media)} } },
        Inline::Link { url, content } => rsx! {
            a { key: "{key}", href: "{url}", target: "_blank", rel: "noopener noreferrer", {render_inlines(content, media)} }
        },
        Inline::Image { url, alt } => rsx! {
            img {
                key: "{key}",
                class: "md-image",
                src: "{url}",
                alt: "{alt}",
                loading: "lazy",
                referrerpolicy: "no-referrer",
                onload: move |_| {
                    if let Some(media) = media {
                        media.call(());
                    }
                },
            }
        },
        Inline::SoftBreak => rsx! { span { key: "{key}", " " } },
        Inline::HardBreak => rsx! { br { key: "{key}" } },
    }
}

/// A code block: its language, a copy button, and the code, highlighted
/// once its closing fence has arrived and its language is known.
#[component]
fn CodeBlock(lang: Option<String>, text: String, closed: bool) -> Element {
    // Some(true) after a copy, Some(false) when the browser refused it.
    let mut copied: Signal<Option<bool>> = use_signal(|| None);
    let spans = if closed { lang.as_deref().and_then(|lang| highlight::highlight(lang, &text)) } else { None };
    let label = lang.clone().unwrap_or_default();
    let text_for_copy = text.clone();
    let copy = move |_| {
        let text = text_for_copy.clone();
        spawn(async move {
            let script = format!(
                "await navigator.clipboard.writeText({}); return true;",
                serde_json::to_string(&text).unwrap_or_default()
            );
            copied.set(Some(document::eval(&script).await.is_ok()));
            #[cfg(feature = "web")]
            {
                gloo_timers::future::TimeoutFuture::new(1500).await;
                copied.set(None);
            }
        });
    };
    let copy_label = match copied() {
        Some(true) => "Copied",
        Some(false) => "Couldn't copy",
        None => "Copy",
    };
    let highlighted = spans.is_some();
    rsx! {
        div { class: "md-code",
            div { class: "md-code-bar",
                span { class: "md-code-lang", "{label}" }
                button { class: "md-code-copy", r#type: "button", onclick: copy, "{copy_label}" }
            }
            pre { class: if highlighted { "md-pre hl-code" } else { "md-pre" },
                code {
                    if let Some(spans) = spans {
                        {render_spans(&spans)}
                    } else {
                        "{text}"
                    }
                }
            }
        }
    }
}

fn render_spans(spans: &[Span]) -> Element {
    rsx! {
        {spans.iter().enumerate().map(|(i, span)| match span {
            Span::Text(text) => rsx! { span { key: "{i}", "{text}" } },
            Span::Scoped { classes, children } => rsx! { span { key: "{i}", class: "{classes}", {render_spans(children)} } },
        })}
    }
}

#[cfg(test)]
mod tests;
