//! The model's replies as markdown (SME-30): a pure parse step into
//! smelt's own node tree, and the `Markdown` component that renders that
//! tree as Dioxus elements.
//!
//! The reply is untrusted text (the model may echo a web page it read),
//! so nothing here builds an HTML string or sets `dangerous_inner_html`:
//! raw HTML in a reply comes out as literal text, a link only becomes an
//! `<a>` for `http`, `https` and `mailto`, and an image only loads for
//! `http`/`https`. See docs/frontend.md, "Markdown in replies", for what
//! this boundary covers and what it doesn't.

use dioxus::prelude::*;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

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
    for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
        builder.event(event, source.get(range).unwrap_or(""));
    }
    builder.finish()
}

/// Whether a link or image destination is one smelt will follow.
fn allowed_url(url: &str, schemes: &[&str]) -> bool {
    url.split_once(':')
        .is_some_and(|(scheme, _)| schemes.iter().any(|s| scheme.eq_ignore_ascii_case(s)))
}

/// Whether a fenced code block's source (opening fence to end) ends with a
/// closing fence at least as long as its opening one.
fn fence_closed(raw: &str) -> bool {
    // Inside a quote, every line after the first keeps its `>` markers.
    let strip = |line: &str| line.trim_start_matches(|c| c == '>' || c == ' ' || c == '\t').trim_end().to_string();
    let lines: Vec<String> = raw.lines().map(strip).collect();
    let (Some(first), Some(last)) = (lines.first(), lines.last()) else {
        return false;
    };
    if lines.len() < 2 {
        return false;
    }
    let Some(fence) = first.chars().next().filter(|c| *c == '`' || *c == '~') else {
        return false;
    };
    let open = first.chars().take_while(|c| *c == fence).count();
    last.chars().count() >= open && last.chars().all(|c| c == fence)
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
    Code { lang: Option<String>, text: String, closed: bool },
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
    fn event(&mut self, event: Event, raw: &str) {
        match event {
            Event::Start(tag) => self.start(tag, raw),
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

    fn start(&mut self, tag: Tag, raw: &str) {
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
                closed: fence_closed(raw),
            },
            Tag::CodeBlock(CodeBlockKind::Indented) => Container::Code { lang: None, text: String::new(), closed: true },
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
                        Tag::Link { dest_url, .. } => Container::Link {
                            url: allowed_url(&dest_url, &["http", "https", "mailto"]).then(|| dest_url.to_string()),
                            content: Vec::new(),
                        },
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
            Container::Code { lang, text, closed } => self.block(Block::Code { lang, text, closed }),
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
mod tests {
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

    #[test]
    fn test_very_deep_nesting_and_long_input_parse() {
        let deep = format!("{}x", "> ".repeat(500));
        assert!(!parse(&deep).is_empty());
        let long = "word ".repeat(20_000);
        assert_eq!(parse(&long).len(), 1);
    }
}
