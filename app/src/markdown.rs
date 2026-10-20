//! Minimal Markdown for agent messages: paragraphs, `#` headings, `-`/`*` and
//! `1.` lists, fenced code, and inline `**bold**`, `*italic*`, `` `code` ``.
//! Builds elements directly (never `inner_html`), so model output can't
//! inject markup. Anything unrecognised stays plain text.

use leptos::prelude::*;

#[derive(Clone, Debug, PartialEq)]
pub enum Inline {
    Text(String),
    Bold(String),
    Italic(String),
    Code(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    Para(Vec<Inline>),
    Heading(Vec<Inline>),
    Bullets(Vec<Vec<Inline>>),
    Numbered(Vec<Vec<Inline>>),
    Code(String),
}

fn bullet(line: &str) -> Option<&str> {
    line.strip_prefix("- ").or_else(|| line.strip_prefix("* "))
}

fn numbered(line: &str) -> Option<&str> {
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    line[digits..]
        .strip_prefix(". ")
        .or_else(|| line[digits..].strip_prefix(") "))
}

/// Split `s` into blocks.
pub fn parse(s: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let mut para: Vec<&str> = Vec::new();
    let mut lines = s.lines().peekable();
    let flush = |para: &mut Vec<&str>, out: &mut Vec<Block>| {
        if !para.is_empty() {
            out.push(Block::Para(inline(&para.join(" "))));
            para.clear();
        }
    };
    while let Some(raw) = lines.next() {
        let line = raw.trim();
        if line.is_empty() {
            flush(&mut para, &mut out);
        } else if line.starts_with("```") {
            flush(&mut para, &mut out);
            let mut code = Vec::new();
            for l in lines.by_ref() {
                if l.trim_start().starts_with("```") {
                    break;
                }
                code.push(l);
            }
            out.push(Block::Code(code.join("\n")));
        } else if let Some(h) = line.strip_prefix('#') {
            flush(&mut para, &mut out);
            out.push(Block::Heading(inline(h.trim_start_matches('#').trim())));
        } else if bullet(line).is_some() || numbered(line).is_some() {
            flush(&mut para, &mut out);
            let is_bullet = bullet(line).is_some();
            let item: fn(&str) -> Option<&str> = if is_bullet { bullet } else { numbered };
            let mut items = vec![inline(item(line).unwrap_or(line))];
            while let Some(next) = lines.peek().map(|l| l.trim()) {
                match item(next) {
                    Some(text) => {
                        items.push(inline(text));
                        lines.next();
                    }
                    None => break,
                }
            }
            out.push(if is_bullet {
                Block::Bullets(items)
            } else {
                Block::Numbered(items)
            });
        } else {
            para.push(line);
        }
    }
    flush(&mut para, &mut out);
    out
}

/// Inline spans. Unclosed markers are kept as text.
pub fn inline(s: &str) -> Vec<Inline> {
    let mut out = Vec::new();
    let mut text = String::new();
    let mut rest = s;
    while !rest.is_empty() {
        let span = [("`", 1usize), ("**", 2), ("*", 1)]
            .iter()
            .find_map(|&(m, n)| {
                let body = rest.strip_prefix(m)?;
                let end = body.find(m)?;
                (end > 0 && !body.starts_with(' ')).then(|| (m, &body[..end], n + end + n))
            });
        match span {
            Some((m, body, len)) => {
                if !text.is_empty() {
                    out.push(Inline::Text(std::mem::take(&mut text)));
                }
                out.push(match m {
                    "`" => Inline::Code(body.into()),
                    "**" => Inline::Bold(body.into()),
                    _ => Inline::Italic(body.into()),
                });
                rest = &rest[len..];
            }
            None => {
                let ch = rest.chars().next().expect("rest is non-empty");
                text.push(ch);
                rest = &rest[ch.len_utf8()..];
            }
        }
    }
    if !text.is_empty() {
        out.push(Inline::Text(text));
    }
    out
}

fn spans(items: Vec<Inline>) -> impl IntoView {
    items
        .into_iter()
        .map(|i| match i {
            Inline::Text(t) => t.into_any(),
            Inline::Bold(t) => view! { <strong>{t}</strong> }.into_any(),
            Inline::Italic(t) => view! { <em>{t}</em> }.into_any(),
            Inline::Code(t) => view! { <code>{t}</code> }.into_any(),
        })
        .collect_view()
}

/// Render `text` as Markdown inside a `div.md`.
#[component]
pub fn Markdown(#[prop(into)] text: String) -> impl IntoView {
    let blocks = parse(&text)
        .into_iter()
        .map(|b| match b {
            Block::Para(i) => view! { <p>{spans(i)}</p> }.into_any(),
            Block::Heading(i) => view! { <p class="md-h">{spans(i)}</p> }.into_any(),
            Block::Bullets(items) => view! {
                <ul>{items.into_iter().map(|i| view! { <li>{spans(i)}</li> }).collect_view()}</ul>
            }
            .into_any(),
            Block::Numbered(items) => view! {
                <ol>{items.into_iter().map(|i| view! { <li>{spans(i)}</li> }).collect_view()}</ol>
            }
            .into_any(),
            Block::Code(c) => view! { <pre><code>{c}</code></pre> }.into_any(),
        })
        .collect_view();
    view! { <div class="md">{blocks}</div> }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Inline::*;

    #[test]
    fn planner_message() {
        let b = parse(
            "**Plan ready.** The codebase is small.\n\n**Task t1:** Add filter\n- Extend `list()`\n- Parse `?done`\n\n1. one\n2. two",
        );
        assert_eq!(
            b,
            vec![
                Block::Para(vec![
                    Bold("Plan ready.".into()),
                    Text(" The codebase is small.".into())
                ]),
                Block::Para(vec![Bold("Task t1:".into()), Text(" Add filter".into())]),
                Block::Bullets(vec![
                    vec![Text("Extend ".into()), Code("list()".into())],
                    vec![Text("Parse ".into()), Code("?done".into())],
                ]),
                Block::Numbered(vec![vec![Text("one".into())], vec![Text("two".into())]]),
            ]
        );
    }

    #[test]
    fn literals_survive() {
        assert_eq!(inline("2 * 3 * 4"), vec![Text("2 * 3 * 4".into())]);
        assert_eq!(inline("a **b"), vec![Text("a **b".into())]);
        assert_eq!(
            inline("<script>x</script>"),
            vec![Text("<script>x</script>".into())]
        );
        assert_eq!(
            inline("*em* and é"),
            vec![Italic("em".into()), Text(" and é".into())]
        );
    }

    #[test]
    fn headings_and_code_fences() {
        assert_eq!(
            parse("## Summary\n```\nlet x = 1;\n```\nafter"),
            vec![
                Block::Heading(vec![Text("Summary".into())]),
                Block::Code("let x = 1;".into()),
                Block::Para(vec![Text("after".into())]),
            ]
        );
    }
}
