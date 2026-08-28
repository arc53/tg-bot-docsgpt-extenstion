//! Turning DocsGPT markdown into what Telegram accepts: Rich Markdown
//! (near-GFM) first, MarkdownV2 as a fallback, plain text as the last resort.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

use crate::docsgpt::Source;

pub const TG_TEXT_LIMIT: usize = 4096;
pub const RICH_TEXT_LIMIT: usize = 30_000;

/// Remove markdown images, returning the text without them and the image URLs.
pub fn strip_images(md: &str) -> (String, Vec<String>) {
    let re = regex::Regex::new(r#"!\[[^\]]*\]\(\s*<?([^)\s>]+)>?(?:\s+"[^"]*")?\s*\)"#).unwrap();
    let mut urls = Vec::new();
    let replaced = re.replace_all(md, |caps: &regex::Captures| {
        let url = caps[1].to_string();
        if (url.starts_with("http://") || url.starts_with("https://")) && !urls.contains(&url) {
            urls.push(url);
        }
        String::new()
    });
    // Drop lines that became empty because they only held an image.
    let mut out = String::with_capacity(replaced.len());
    let mut prev_blank = false;
    for line in replaced.lines() {
        let blank = line.trim().is_empty();
        if blank && prev_blank {
            continue;
        }
        out.push_str(line);
        out.push('\n');
        prev_blank = blank;
    }
    (out.trim().to_string(), urls)
}

/// Escape for MarkdownV2 running text.
pub fn escape_v2(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if matches!(
            c,
            '_' | '*'
                | '['
                | ']'
                | '('
                | ')'
                | '~'
                | '`'
                | '>'
                | '#'
                | '+'
                | '-'
                | '='
                | '|'
                | '{'
                | '}'
                | '.'
                | '!'
                | '\\'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn escape_v2_code(s: &str) -> String {
    s.replace('\\', "\\\\").replace('`', "\\`")
}

fn escape_v2_url(s: &str) -> String {
    s.replace('\\', "\\\\").replace(')', "\\)")
}

struct V2 {
    blocks: Vec<String>,
    cur: String,
    lists: Vec<Option<u64>>,
    quote: usize,
    in_code: bool,
    link: Option<String>,
    link_text: String,
    table: Option<TableState>,
    in_image: bool,
}

#[derive(Default)]
struct TableState {
    rows: Vec<Vec<String>>,
    cell: String,
    in_head: bool,
}

impl V2 {
    fn flush(&mut self) {
        let text = self.cur.trim_end().to_string();
        if !text.trim().is_empty() {
            let text = if self.quote > 0 {
                text.lines()
                    .map(|l| format!(">{l}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                text
            };
            self.blocks.push(text);
        }
        self.cur.clear();
    }

    fn push_text(&mut self, s: &str) {
        if self.in_image {
            return;
        }
        if let Some(t) = &mut self.table {
            t.cell.push_str(s);
            return;
        }
        if self.link.is_some() {
            self.link_text.push_str(&escape_v2(s));
            return;
        }
        if self.in_code {
            self.cur.push_str(&escape_v2_code(s));
        } else {
            self.cur.push_str(&escape_v2(s));
        }
    }

    fn push_raw(&mut self, s: &str) {
        if self.in_image {
            return;
        }
        if self.table.is_some() {
            return;
        }
        if self.link.is_some() {
            self.link_text.push_str(s);
        } else {
            self.cur.push_str(s);
        }
    }

    fn newline_for_item(&mut self) {
        if !self.cur.is_empty() && !self.cur.ends_with('\n') {
            self.cur.push('\n');
        }
    }
}

/// Convert markdown into MarkdownV2 blocks (paragraph-level units).
pub fn markdown_v2_blocks(md: &str) -> Vec<String> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_FOOTNOTES);
    let parser = Parser::new_ext(md, opts);
    let mut st = V2 {
        blocks: vec![],
        cur: String::new(),
        lists: vec![],
        quote: 0,
        in_code: false,
        link: None,
        link_text: String::new(),
        table: None,
        in_image: false,
    };
    for ev in parser {
        match ev {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    if st.lists.is_empty() && st.table.is_none() {
                        st.flush();
                    } else if !st.lists.is_empty()
                        && !st.cur.is_empty()
                        && !st.cur.ends_with('\n')
                        && !st.cur.ends_with(' ')
                    {
                        st.cur.push('\n');
                    }
                }
                Tag::Heading { level, .. } => {
                    st.flush();
                    let _ = level;
                    st.push_raw("*");
                }
                Tag::BlockQuote(_) => {
                    st.flush();
                    st.quote += 1;
                }
                Tag::CodeBlock(kind) => {
                    st.flush();
                    st.in_code = true;
                    let lang = match kind {
                        CodeBlockKind::Fenced(l) => l.to_string(),
                        CodeBlockKind::Indented => String::new(),
                    };
                    let lang: String = lang
                        .chars()
                        .filter(|c| {
                            c.is_ascii_alphanumeric() || *c == '_' || *c == '-' || *c == '+'
                        })
                        .collect();
                    st.push_raw(&format!("```{lang}\n"));
                }
                Tag::List(start) => {
                    if st.lists.is_empty() {
                        st.flush();
                    }
                    st.lists.push(start);
                }
                Tag::Item => {
                    st.newline_for_item();
                    let depth = st.lists.len().saturating_sub(1);
                    let indent = "  ".repeat(depth);
                    let marker = match st.lists.last_mut() {
                        Some(Some(n)) => {
                            let m = format!("{n}\\. ");
                            *n += 1;
                            m
                        }
                        _ => "• ".to_string(),
                    };
                    st.push_raw(&format!("{indent}{marker}"));
                }
                Tag::Emphasis => st.push_raw("_"),
                Tag::Strong => st.push_raw("*"),
                Tag::Strikethrough => st.push_raw("~"),
                Tag::Link { dest_url, .. } => {
                    st.link = Some(dest_url.to_string());
                    st.link_text.clear();
                }
                Tag::Image { .. } => st.in_image = true,
                Tag::Table(_) => {
                    st.flush();
                    st.table = Some(TableState::default());
                }
                Tag::TableHead => {
                    if let Some(t) = &mut st.table {
                        t.in_head = true;
                        t.rows.push(vec![]);
                    }
                }
                Tag::TableRow => {
                    if let Some(t) = &mut st.table {
                        t.rows.push(vec![]);
                    }
                }
                Tag::TableCell => {
                    if let Some(t) = &mut st.table {
                        t.cell.clear();
                    }
                }
                Tag::FootnoteDefinition(label) => {
                    st.flush();
                    st.push_raw(&format!("\\[{}\\] ", escape_v2(&label)));
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph if st.lists.is_empty() && st.table.is_none() => {
                    st.flush();
                }
                TagEnd::Heading(_) => {
                    st.push_raw("*");
                    st.flush();
                }
                TagEnd::BlockQuote(_) => {
                    st.flush();
                    st.quote = st.quote.saturating_sub(1);
                }
                TagEnd::CodeBlock => {
                    if !st.cur.ends_with('\n') {
                        st.cur.push('\n');
                    }
                    st.push_raw("```");
                    st.in_code = false;
                    st.flush();
                }
                TagEnd::List(_) => {
                    st.lists.pop();
                    if st.lists.is_empty() {
                        st.flush();
                    }
                }
                TagEnd::Item => {}
                TagEnd::Emphasis => st.push_raw("_"),
                TagEnd::Strong => st.push_raw("*"),
                TagEnd::Strikethrough => st.push_raw("~"),
                TagEnd::Link => {
                    if let Some(url) = st.link.take() {
                        let text = if st.link_text.trim().is_empty() {
                            escape_v2(&url)
                        } else {
                            st.link_text.clone()
                        };
                        let piece = if url.starts_with("http://")
                            || url.starts_with("https://")
                            || url.starts_with("tg://")
                        {
                            format!("[{}]({})", text, escape_v2_url(&url))
                        } else {
                            text
                        };
                        st.link_text.clear();
                        st.cur.push_str(&piece);
                    }
                }
                TagEnd::Image => st.in_image = false,
                TagEnd::Table => {
                    if let Some(t) = st.table.take() {
                        let rendered = render_table_monospace(&t.rows);
                        st.cur
                            .push_str(&format!("```\n{}\n```", escape_v2_code(&rendered)));
                        st.flush();
                    }
                }
                TagEnd::TableHead => {
                    if let Some(t) = &mut st.table {
                        t.in_head = false;
                    }
                }
                TagEnd::TableRow => {}
                TagEnd::TableCell => {
                    if let Some(t) = &mut st.table {
                        let cell = t.cell.trim().to_string();
                        if let Some(row) = t.rows.last_mut() {
                            row.push(cell);
                        }
                        t.cell.clear();
                    }
                }
                TagEnd::FootnoteDefinition => st.flush(),
                _ => {}
            },
            Event::Text(t) => st.push_text(&t),
            Event::Code(c) => {
                if st.table.is_some() {
                    st.push_text(&c);
                } else {
                    st.push_raw(&format!("`{}`", escape_v2_code(&c)));
                }
            }
            Event::Html(h) | Event::InlineHtml(h) => st.push_text(&h),
            Event::FootnoteReference(l) => st.push_raw(&format!("\\[{}\\]", escape_v2(&l))),
            Event::SoftBreak => {
                if st.table.is_some() {
                    st.push_text(" ");
                } else {
                    st.push_raw("\n");
                }
            }
            Event::HardBreak => st.push_raw("\n"),
            Event::Rule => {
                st.flush();
                st.blocks.push("———".into());
            }
            Event::TaskListMarker(done) => st.push_raw(if done { "☑ " } else { "☐ " }),
            Event::InlineMath(m) => st.push_raw(&format!("`{}`", escape_v2_code(&m))),
            Event::DisplayMath(m) => {
                st.flush();
                st.push_raw(&format!("```\n{}\n```", escape_v2_code(&m)));
                st.flush();
            }
        }
    }
    st.flush();
    st.blocks
}

fn render_table_monospace(rows: &[Vec<String>]) -> String {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    if cols == 0 {
        return String::new();
    }
    let mut widths = vec![0usize; cols];
    for r in rows {
        for (i, c) in r.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count().min(40));
        }
    }
    let mut out = String::new();
    for (ri, r) in rows.iter().enumerate() {
        let mut line = String::new();
        for (i, width) in widths.iter().enumerate() {
            let cell = r.get(i).map(String::as_str).unwrap_or("");
            let cell: String = cell.chars().take(40).collect();
            let pad = width.saturating_sub(cell.chars().count());
            line.push_str(&cell);
            line.push_str(&" ".repeat(pad));
            if i + 1 < cols {
                line.push_str(" | ");
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
        if ri == 0 && rows.len() > 1 {
            let sep: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
            out.push_str(&sep.join("-|-"));
            out.push('\n');
        }
    }
    out.trim_end().to_string()
}

/// Pack MarkdownV2 blocks into messages of at most `limit` characters,
/// splitting oversized code blocks without breaking the fence.
pub fn pack_blocks(blocks: &[String], limit: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for block in blocks {
        let pieces: Vec<String> = if block.chars().count() > limit {
            split_block(block, limit)
        } else {
            vec![block.clone()]
        };
        for piece in pieces {
            let extra = if cur.is_empty() { 0 } else { 2 };
            if cur.chars().count() + extra + piece.chars().count() > limit {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                cur = piece;
            } else {
                if !cur.is_empty() {
                    cur.push_str("\n\n");
                }
                cur.push_str(&piece);
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn split_block(block: &str, limit: usize) -> Vec<String> {
    if block.starts_with("```") {
        let first_nl = block.find('\n').unwrap_or(block.len());
        let fence = &block[..first_nl];
        let body = block[first_nl..]
            .trim_start_matches('\n')
            .trim_end_matches("```")
            .trim_end_matches('\n');
        let overhead = fence.chars().count() + 8;
        let chunks = split_plain(body, limit.saturating_sub(overhead).max(64));
        return chunks
            .into_iter()
            .map(|c| format!("{fence}\n{c}\n```"))
            .collect();
    }
    split_plain(block, limit)
}

/// Split plain text at newlines, then spaces, then hard — character-aware.
pub fn split_plain(text: &str, limit: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut rest: Vec<char> = text.chars().collect();
    while !rest.is_empty() {
        if rest.len() <= limit {
            chunks.push(rest.iter().collect());
            break;
        }
        let window = &rest[..limit];
        let mut cut = window.iter().rposition(|c| *c == '\n');
        if cut.is_none_or(|c| c < limit / 4) {
            cut = window.iter().rposition(|c| *c == ' ').or(cut);
        }
        let cut = cut.filter(|c| *c > 0).unwrap_or(limit);
        let piece: String = rest[..cut].iter().collect();
        chunks.push(piece.trim_end().to_string());
        let skip = if cut < rest.len() && (rest[cut] == '\n' || rest[cut] == ' ') {
            cut + 1
        } else {
            cut
        };
        rest.drain(..skip);
    }
    chunks
        .into_iter()
        .filter(|c| !c.trim().is_empty())
        .collect()
}

/// Whole markdown → MarkdownV2 messages.
pub fn markdown_v2_messages(md: &str, limit: usize) -> Vec<String> {
    let blocks = markdown_v2_blocks(md);
    pack_blocks(&blocks, limit)
}

/// Plain-text messages (last resort).
pub fn plain_messages(text: &str, limit: usize) -> Vec<String> {
    split_plain(text, limit)
}

fn clean_title(t: &str) -> String {
    let t = t.trim();
    let t: String = t
        .chars()
        .map(|c| if c == '[' || c == ']' { ' ' } else { c })
        .collect();
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    crate::util::truncate_chars(if t.is_empty() { "Source" } else { &t }, 80)
}

/// Collapsible sources block in Rich Markdown.
pub fn rich_sources(sources: &[Source]) -> String {
    if sources.is_empty() {
        return String::new();
    }
    let mut out = format!(
        "\n\n<details><summary>Sources ({})</summary>\n\n",
        sources.len()
    );
    for s in sources.iter().take(20) {
        let title = clean_title(&s.title);
        match &s.url {
            Some(u) => out.push_str(&format!("- [{title}]({u})\n")),
            None => out.push_str(&format!("- {title}\n")),
        }
    }
    out.push_str("\n</details>");
    out
}

/// Sources as a MarkdownV2 block.
pub fn v2_sources(sources: &[Source]) -> Option<String> {
    if sources.is_empty() {
        return None;
    }
    let mut out = String::from("*Sources*\n");
    for s in sources.iter().take(20) {
        let title = escape_v2(&clean_title(&s.title));
        match &s.url {
            Some(u) if u.starts_with("http") => {
                out.push_str(&format!("• [{title}]({})\n", escape_v2_url(u)))
            }
            _ => out.push_str(&format!("• {title}\n")),
        }
    }
    Some(out.trim_end().to_string())
}

/// Sources as plain text.
pub fn plain_sources(sources: &[Source]) -> Option<String> {
    if sources.is_empty() {
        return None;
    }
    let mut out = String::from("Sources:\n");
    for s in sources.iter().take(20) {
        match &s.url {
            Some(u) => out.push_str(&format!("• {} — {}\n", clean_title(&s.title), u)),
            None => out.push_str(&format!("• {}\n", clean_title(&s.title))),
        }
    }
    Some(out.trim_end().to_string())
}

/// Strip markdown syntax for a plain-text rendering.
pub fn markdown_to_plain(md: &str) -> String {
    let blocks = markdown_v2_blocks(md);
    let joined = blocks.join("\n\n");
    unescape_v2(&joined)
}

fn unescape_v2(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
            continue;
        }
        if matches!(c, '*' | '_' | '~' | '`') {
            continue;
        }
        out.push(c);
    }
    // Links: [text](url) → text (url)
    let re = regex::Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap();
    re.replace_all(&out, "$1 ($2)").into_owned()
}

/// Rich Markdown cannot exceed the limit; cut at a paragraph boundary.
pub fn clamp_rich(md: &str, limit: usize) -> String {
    if md.chars().count() <= limit {
        return md.to_string();
    }
    let mut out = String::new();
    for para in md.split("\n\n") {
        if out.chars().count() + para.chars().count() + 2 > limit.saturating_sub(4) {
            break;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(para);
    }
    if out.is_empty() {
        out = crate::util::truncate_chars(md, limit.saturating_sub(4));
    }
    out.push_str("\n\n…");
    out
}

/// Close an unterminated code fence so a partial draft still parses.
pub fn close_open_fence(md: &str) -> String {
    let fences = md
        .lines()
        .filter(|l| l.trim_start().starts_with("```"))
        .count();
    if fences % 2 == 1 {
        format!("{md}\n```")
    } else {
        md.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_images() {
        let (text, urls) = strip_images("Look:\n\n![cat](https://x.test/a.png)\n\nDone.");
        assert_eq!(text, "Look:\n\nDone.");
        assert_eq!(urls, vec!["https://x.test/a.png"]);
        let (t2, u2) = strip_images("![a](https://x/1.png \"cap\") and ![b](https://x/2.png)");
        assert_eq!(t2, "and");
        assert_eq!(u2.len(), 2);
    }

    #[test]
    fn escapes_markdown_v2() {
        assert_eq!(escape_v2("a.b-c!"), "a\\.b\\-c\\!");
        let blocks =
            markdown_v2_blocks("Hello **world** and _it_ with `x.y` [link](https://e.com/a)");
        assert_eq!(
            blocks,
            vec!["Hello *world* and _it_ with `x.y` [link](https://e.com/a)"]
        );
    }

    #[test]
    fn renders_headings_lists_code() {
        let md = "# Title\n\n- one\n- two\n\n1. a\n2. b\n\n```python\nprint('x')\n```\n\n> quote";
        let blocks = markdown_v2_blocks(md);
        assert_eq!(blocks[0], "*Title*");
        assert_eq!(blocks[1], "• one\n• two");
        assert_eq!(blocks[2], "1\\. a\n2\\. b");
        assert_eq!(blocks[3], "```python\nprint('x')\n```");
        assert_eq!(blocks[4], ">quote");
    }

    #[test]
    fn renders_table_as_monospace() {
        let md = "| Cat | Trait |\n|---|---|\n| Tom | Curious |";
        let blocks = markdown_v2_blocks(md);
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].starts_with("```\nCat | Trait"));
        assert!(blocks[0].contains("Tom | Curious"));
    }

    #[test]
    fn packs_and_splits() {
        let blocks = vec!["a".repeat(10), "b".repeat(10), "c".repeat(30)];
        let msgs = pack_blocks(&blocks, 25);
        assert_eq!(msgs[0], format!("{}\n\n{}", "a".repeat(10), "b".repeat(10)));
        assert_eq!(msgs.len(), 3);
        let code = format!(
            "```py\n{}\n```",
            (0..50)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let pieces = split_block(&code, 120);
        assert!(pieces.len() > 1);
        for p in &pieces {
            assert!(p.starts_with("```py\n") && p.ends_with("\n```"));
            assert!(p.chars().count() <= 120);
        }
    }

    #[test]
    fn split_plain_prefers_newlines() {
        let text = "one two three\nfour five six\nseven";
        let parts = split_plain(text, 16);
        assert_eq!(parts, vec!["one two three", "four five six", "seven"]);
        let long = "x".repeat(50);
        assert_eq!(split_plain(&long, 20).len(), 3);
    }

    #[test]
    fn sources_blocks() {
        let s = vec![
            Source {
                title: "Doc [1]".into(),
                url: Some("https://a.b/c".into()),
            },
            Source {
                title: "".into(),
                url: None,
            },
        ];
        let rich = rich_sources(&s);
        assert!(rich.contains("<details><summary>Sources (2)</summary>"));
        assert!(rich.contains("- [Doc 1](https://a.b/c)"));
        assert!(rich.contains("- Source\n"));
        let v2 = v2_sources(&s).unwrap();
        assert!(v2.starts_with("*Sources*\n• [Doc 1](https://a.b/c)"));
        assert!(v2_sources(&[]).is_none());
    }

    #[test]
    fn plain_rendering() {
        assert_eq!(
            markdown_to_plain("**Bold** and [x](https://y.z)"),
            "Bold and x (https://y.z)"
        );
    }

    #[test]
    fn closes_fence() {
        assert_eq!(close_open_fence("```py\nx"), "```py\nx\n```");
        assert_eq!(close_open_fence("```py\nx\n```"), "```py\nx\n```");
    }

    #[test]
    fn clamps_rich() {
        let md = format!("{}\n\n{}", "a".repeat(10), "b".repeat(10));
        let c = clamp_rich(&md, 18);
        assert_eq!(c, format!("{}\n\n…", "a".repeat(10)));
    }
}
