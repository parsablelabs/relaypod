use std::sync::Arc;

use eframe::egui;
use egui::text::LayoutJob;
use egui::{Color32, CornerRadius, FontFamily, FontId, Margin, Stroke, TextFormat};

/// Renders a subset of Markdown (headings, fenced code blocks, blockquotes,
/// bullet/numbered lists, horizontal rules, inline code, bold, italic,
/// strikethrough, links, and `@mention` highlights) directly in an `egui::Ui`.
pub fn render_markdown(ui: &mut egui::Ui, text: &str) {
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        // Fenced code block: ```lang
        if let Some(rest) = trimmed.strip_prefix("```") {
            let lang = rest.trim();
            i += 1;
            let mut code_lines = Vec::new();
            while i < lines.len() && !lines[i].trim().starts_with("```") {
                code_lines.push(lines[i]);
                i += 1;
            }
            if i < lines.len() {
                i += 1; // consume closing ```
            }
            render_code_block(ui, lang, &code_lines.join("\n"));
            ui.add_space(4.0);
            continue;
        }

        // Blank line -> small paragraph break
        if trimmed.is_empty() {
            ui.add_space(4.0);
            i += 1;
            continue;
        }

        // Horizontal rule
        if is_horizontal_rule(trimmed) {
            ui.separator();
            i += 1;
            continue;
        }

        // Headings: #, ##, ###
        if let Some((level, content)) = parse_heading(trimmed) {
            let size = match level {
                1 => 18.0,
                2 => 16.0,
                _ => 14.5,
            };
            let color = ui.visuals().strong_text_color();
            render_inline_text(ui, content, size, color, true);
            ui.add_space(2.0);
            i += 1;
            continue;
        }

        // Blockquote (> ...)
        if trimmed.starts_with('>') {
            let mut quote_lines = Vec::new();
            while i < lines.len() {
                let t = lines[i].trim();
                if let Some(rest) = t.strip_prefix('>') {
                    quote_lines.push(rest.trim_start());
                    i += 1;
                } else {
                    break;
                }
            }
            render_blockquote(ui, &quote_lines);
            ui.add_space(3.0);
            continue;
        }

        // Unordered list (- item or * item)
        if let Some(item) = parse_unordered_item(line) {
            render_list_item(ui, "•", item);
            i += 1;
            continue;
        }

        // Ordered list (1. item)
        if let Some((marker, item)) = parse_ordered_item(line) {
            render_list_item(ui, &marker, item);
            i += 1;
            continue;
        }

        // Regular paragraph line
        let color = ui.visuals().text_color();
        render_inline_text(ui, line, 13.5, color, false);
        i += 1;
    }
}

fn is_horizontal_rule(trimmed: &str) -> bool {
    if trimmed.len() < 3 {
        return false;
    }
    let first = trimmed.as_bytes()[0];
    if first != b'-' && first != b'*' && first != b'_' {
        return false;
    }
    trimmed.bytes().all(|b| b == first || b == b' ')
        && trimmed.bytes().filter(|&b| b == first).count() >= 3
}

fn parse_heading(trimmed: &str) -> Option<(usize, &str)> {
    let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&hashes) {
        let rest = &trimmed[hashes..];
        if let Some(content) = rest.strip_prefix(' ') {
            return Some((hashes, content.trim()));
        }
    }
    None
}

fn parse_unordered_item(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
}

fn parse_ordered_item(line: &str) -> Option<(String, &str)> {
    let trimmed = line.trim_start();
    let digit_count = trimmed.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digit_count == 0 {
        return None;
    }
    let rest = &trimmed[digit_count..];
    let item = rest.strip_prefix(". ")?;
    Some((format!("{}.", &trimmed[..digit_count]), item))
}

fn render_code_block(ui: &mut egui::Ui, lang: &str, code: &str) {
    let bg = if ui.visuals().dark_mode {
        Color32::from_rgb(20, 22, 27)
    } else {
        Color32::from_rgb(238, 240, 244)
    };
    let border = if ui.visuals().dark_mode {
        Color32::from_rgb(50, 55, 66)
    } else {
        Color32::from_rgb(210, 214, 222)
    };

    egui::Frame::new()
        .fill(bg)
        .stroke(Stroke::new(1.0_f32, border))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            if !lang.is_empty() {
                ui.label(
                    egui::RichText::new(lang)
                        .size(10.5)
                        .monospace()
                        .color(ui.visuals().weak_text_color()),
                );
                ui.add_space(2.0);
            }
            ui.add(
                egui::Label::new(
                    egui::RichText::new(code)
                        .monospace()
                        .size(12.5)
                        .color(ui.visuals().strong_text_color()),
                )
                .wrap(),
            );
        });
}

fn render_blockquote(ui: &mut egui::Ui, lines: &[&str]) {
    let accent = Color32::from_rgb(99, 132, 189);
    let bg = if ui.visuals().dark_mode {
        Color32::from_rgba_unmultiplied(99, 132, 189, 18)
    } else {
        Color32::from_rgba_unmultiplied(99, 132, 189, 24)
    };

    let resp = egui::Frame::new()
        .fill(bg)
        .corner_radius(CornerRadius::same(4))
        .inner_margin(Margin {
            left: 10,
            right: 8,
            top: 4,
            bottom: 4,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let color = ui.visuals().weak_text_color();
            for line in lines {
                render_inline_text(ui, line, 13.0, color, false);
            }
        });

    let rect = resp.response.rect;
    let bar = egui::Rect::from_min_max(
        rect.left_top(),
        egui::pos2(rect.left() + 3.0, rect.bottom()),
    );
    ui.painter().rect_filled(bar, 2.0, accent);
}

fn render_list_item(ui: &mut egui::Ui, marker: &str, content: &str) {
    ui.horizontal_top(|ui| {
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(marker)
                .size(13.5)
                .color(ui.visuals().weak_text_color()),
        );
        let color = ui.visuals().text_color();
        render_inline_text(ui, content, 13.5, color, false);
    });
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LinkSpan {
    section_index: usize,
    url: String,
}

fn render_inline_text(
    ui: &mut egui::Ui,
    text: &str,
    base_size: f32,
    base_color: Color32,
    strong_default: bool,
) {
    let (mut job, links) = build_inline_job(ui, text, base_size, base_color, strong_default);
    if links.is_empty() {
        ui.add(egui::Label::new(job).wrap());
        return;
    }

    job.wrap.max_width = ui.available_width().max(1.0);
    let galley = ui.fonts(|f| f.layout_job(job));
    let resp = ui.add(egui::Label::new(Arc::clone(&galley)).sense(egui::Sense::click()));

    let pointer_pos = resp.hover_pos().or_else(|| resp.interact_pointer_pos());
    if let Some(pos) = pointer_pos {
        let local_pos = pos - resp.rect.min;
        if let Some(url) = hit_test_link(&galley, &links, local_pos) {
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            let clicked = resp.clicked() || resp.middle_clicked();
            resp.on_hover_text(url);
            if clicked {
                ui.ctx().open_url(egui::OpenUrl::new_tab(url));
            }
        }
    }
}

fn hit_test_link<'a>(
    galley: &egui::Galley,
    links: &'a [LinkSpan],
    local_pos: egui::Vec2,
) -> Option<&'a str> {
    let pos = local_pos.to_pos2();
    for row in &galley.rows {
        if !row.rect.contains(pos) {
            continue;
        }
        for glyph in &row.glyphs {
            let left = glyph.pos.x;
            let right = left + glyph.advance_width;
            if pos.x >= left && pos.x <= right {
                let sec = glyph.section_index as usize;
                return links
                    .iter()
                    .find(|link| link.section_index == sec)
                    .map(|link| link.url.as_str());
            }
        }
    }
    None
}

fn is_openable_url(url: &str) -> bool {
    let bytes = url.as_bytes();
    let scheme_len = if bytes.len() > 8 && bytes[..8].eq_ignore_ascii_case(b"https://") {
        8
    } else if bytes.len() > 7 && bytes[..7].eq_ignore_ascii_case(b"http://") {
        7
    } else {
        return false;
    };
    let rest = &url[scheme_len..];
    !rest.is_empty() && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Parses a Markdown link `[label](url)` starting at byte index 0 of `slice`.
/// Supports balanced parentheses inside `url` (e.g. Wikipedia links).
/// Returns `(label, url, total_bytes_consumed)`.
fn parse_markdown_link(slice: &str) -> Option<(&str, &str, usize)> {
    let rest = slice.strip_prefix('[')?;
    let close_bracket = rest.find("](")?;
    let label = &rest[..close_bracket];
    if label.is_empty() {
        return None;
    }
    let url_start = 1 + close_bracket + 2;
    let url_bytes = &slice.as_bytes()[url_start..];
    let mut depth = 0usize;
    let mut close_paren = None;
    for (idx, &b) in url_bytes.iter().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => {
                if depth == 0 {
                    close_paren = Some(idx);
                    break;
                }
                depth -= 1;
            }
            b'\n' | b'\r' => return None,
            _ => {}
        }
    }
    let end_rel = close_paren?;
    let url = slice[url_start..url_start + end_rel].trim();
    Some((label, url, url_start + end_rel + 1))
}

/// Matches a bare `http://` or `https://` URL starting at byte index 0 of `slice`,
/// trimming trailing sentence punctuation and unbalanced closing brackets.
fn match_bare_url(slice: &str) -> Option<&str> {
    let bytes = slice.as_bytes();
    let has_scheme = (bytes.len() > 8 && bytes[..8].eq_ignore_ascii_case(b"https://"))
        || (bytes.len() > 7 && bytes[..7].eq_ignore_ascii_case(b"http://"));
    if !has_scheme {
        return None;
    }

    let mut end = slice.len();
    for (idx, ch) in slice.char_indices() {
        if ch.is_whitespace() || matches!(ch, '<' | '>' | '"' | '\'' | '`') {
            end = idx;
            break;
        }
    }

    let mut url = &slice[..end];
    while let Some(last) = url.chars().next_back() {
        let should_strip = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"' => true,
            ')' => {
                url.bytes().filter(|&b| b == b')').count()
                    > url.bytes().filter(|&b| b == b'(').count()
            }
            ']' => {
                url.bytes().filter(|&b| b == b']').count()
                    > url.bytes().filter(|&b| b == b'[').count()
            }
            '}' => {
                url.bytes().filter(|&b| b == b'}').count()
                    > url.bytes().filter(|&b| b == b'{').count()
            }
            _ => false,
        };
        if should_strip {
            url = &url[..url.len() - last.len_utf8()];
        } else {
            break;
        }
    }

    is_openable_url(url).then_some(url)
}

fn build_inline_job(
    ui: &egui::Ui,
    text: &str,
    base_size: f32,
    base_color: Color32,
    strong_default: bool,
) -> (LayoutJob, Vec<LinkSpan>) {
    let mut job = LayoutJob::default();
    let mut links = Vec::new();
    let dark = ui.visuals().dark_mode;
    let strong_color = ui.visuals().strong_text_color();
    let code_bg = if dark {
        Color32::from_rgb(45, 50, 62)
    } else {
        Color32::from_rgb(228, 232, 238)
    };
    let code_fg = if dark {
        Color32::from_rgb(235, 190, 135)
    } else {
        Color32::from_rgb(165, 55, 45)
    };
    let mention_bg = if dark {
        Color32::from_rgba_unmultiplied(88, 140, 235, 48)
    } else {
        Color32::from_rgba_unmultiplied(59, 130, 246, 36)
    };
    let mention_fg = if dark {
        Color32::from_rgb(147, 197, 253)
    } else {
        Color32::from_rgb(29, 78, 216)
    };
    let link_fg = ui.visuals().hyperlink_color;

    let default_fmt = TextFormat {
        font_id: FontId::new(base_size, FontFamily::Proportional),
        color: if strong_default {
            strong_color
        } else {
            base_color
        },
        ..Default::default()
    };

    let link_fmt = TextFormat {
        font_id: FontId::new(base_size, FontFamily::Proportional),
        color: link_fg,
        underline: Stroke::new(1.0_f32, link_fg),
        ..Default::default()
    };

    let bytes = text.as_bytes();
    let mut i = 0;
    let mut plain_start = 0;

    let flush_plain = |job: &mut LayoutJob, start: usize, end: usize| {
        if end > start {
            job.append(&text[start..end], 0.0, default_fmt.clone());
        }
    };

    while i < bytes.len() {
        // Inline code: `...`
        if bytes[i] == b'`' {
            if let Some(rel_end) = text[i + 1..].find('`') {
                flush_plain(&mut job, plain_start, i);
                let inner = &text[i + 1..i + 1 + rel_end];
                job.append(
                    inner,
                    0.0,
                    TextFormat {
                        font_id: FontId::new(base_size - 0.5, FontFamily::Monospace),
                        color: code_fg,
                        background: code_bg,
                        ..Default::default()
                    },
                );
                i = i + 1 + rel_end + 1;
                plain_start = i;
                continue;
            }
        }

        // Bold: **...** or __...__
        if (bytes[i..].starts_with(b"**") || bytes[i..].starts_with(b"__")) && i + 2 < bytes.len() {
            let delim = &text[i..i + 2];
            if let Some(rel_end) = text[i + 2..].find(delim) {
                if rel_end > 0 {
                    flush_plain(&mut job, plain_start, i);
                    let inner = &text[i + 2..i + 2 + rel_end];
                    job.append(
                        inner,
                        0.0,
                        TextFormat {
                            font_id: FontId::new(base_size + 0.5, FontFamily::Proportional),
                            color: strong_color,
                            ..Default::default()
                        },
                    );
                    i = i + 2 + rel_end + 2;
                    plain_start = i;
                    continue;
                }
            }
        }

        // Strikethrough: ~~...~~
        if bytes[i..].starts_with(b"~~") && i + 2 < bytes.len() {
            if let Some(rel_end) = text[i + 2..].find("~~") {
                if rel_end > 0 {
                    flush_plain(&mut job, plain_start, i);
                    let inner = &text[i + 2..i + 2 + rel_end];
                    job.append(
                        inner,
                        0.0,
                        TextFormat {
                            font_id: FontId::new(base_size, FontFamily::Proportional),
                            color: ui.visuals().weak_text_color(),
                            strikethrough: Stroke::new(1.0_f32, ui.visuals().weak_text_color()),
                            ..Default::default()
                        },
                    );
                    i = i + 2 + rel_end + 2;
                    plain_start = i;
                    continue;
                }
            }
        }

        // Italic: *...* or _..._ (not preceded/followed by same char)
        if (bytes[i] == b'*' || bytes[i] == b'_') && i + 1 < bytes.len() {
            let ch = bytes[i] as char;
            let prev_alnum = i > 0 && bytes[i - 1].is_ascii_alphanumeric();
            if !(ch == '_' && prev_alnum) {
                if let Some(rel_end) = text[i + 1..].find(ch) {
                    if rel_end > 0 && !text[i + 1..i + 1 + rel_end].starts_with(' ') {
                        flush_plain(&mut job, plain_start, i);
                        let inner = &text[i + 1..i + 1 + rel_end];
                        job.append(
                            inner,
                            0.0,
                            TextFormat {
                                font_id: FontId::new(base_size, FontFamily::Proportional),
                                color: base_color,
                                italics: true,
                                ..Default::default()
                            },
                        );
                        i = i + 1 + rel_end + 1;
                        plain_start = i;
                        continue;
                    }
                }
            }
        }

        // Markdown link: [label](url)
        if bytes[i] == b'[' {
            if let Some((label, url, consumed)) = parse_markdown_link(&text[i..]) {
                flush_plain(&mut job, plain_start, i);
                let section_index = job.sections.len();
                if is_openable_url(url) {
                    job.append(label, 0.0, link_fmt.clone());
                    links.push(LinkSpan {
                        section_index,
                        url: url.to_string(),
                    });
                } else {
                    job.append(label, 0.0, default_fmt.clone());
                }
                i += consumed;
                plain_start = i;
                continue;
            }
        }

        // Bare URL: https://... or http://...
        if bytes[i] == b'h' || bytes[i] == b'H' {
            let prev_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
            if prev_ok {
                if let Some(url) = match_bare_url(&text[i..]) {
                    flush_plain(&mut job, plain_start, i);
                    let section_index = job.sections.len();
                    job.append(url, 0.0, link_fmt.clone());
                    links.push(LinkSpan {
                        section_index,
                        url: url.to_string(),
                    });
                    i += url.len();
                    plain_start = i;
                    continue;
                }
            }
        }

        // @mention highlight
        if bytes[i] == b'@' {
            let prev_ok = i == 0
                || !(bytes[i - 1].is_ascii_alphanumeric()
                    || bytes[i - 1] == b'_'
                    || bytes[i - 1] == b'-');
            if prev_ok {
                let mut end = i + 1;
                while end < bytes.len()
                    && (bytes[end].is_ascii_alphanumeric()
                        || bytes[end] == b'_'
                        || bytes[end] == b'-')
                {
                    end += 1;
                }
                if end > i + 1 {
                    flush_plain(&mut job, plain_start, i);
                    job.append(
                        &text[i..end],
                        0.0,
                        TextFormat {
                            font_id: FontId::new(base_size, FontFamily::Proportional),
                            color: mention_fg,
                            background: mention_bg,
                            ..Default::default()
                        },
                    );
                    i = end;
                    plain_start = i;
                    continue;
                }
            }
        }

        // Advance by one UTF-8 char
        let ch_len = text[i..].chars().next().map_or(1, char::len_utf8);
        i += ch_len;
    }

    flush_plain(&mut job, plain_start, bytes.len());
    (job, links)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_headings_and_lists() {
        assert_eq!(parse_heading("# Title"), Some((1, "Title")));
        assert_eq!(parse_heading("## Sub"), Some((2, "Sub")));
        assert_eq!(parse_heading("#nospace"), None);
        assert_eq!(parse_unordered_item("- hello"), Some("hello"));
        assert_eq!(parse_unordered_item("  * world"), Some("world"));
        assert_eq!(
            parse_ordered_item("12. step"),
            Some(("12.".to_string(), "step"))
        );
        assert!(is_horizontal_rule("---"));
        assert!(!is_horizontal_rule("--"));
    }

    #[test]
    fn matches_bare_urls_and_trims_punctuation() {
        assert_eq!(
            match_bare_url("https://example.com/pr/42."),
            Some("https://example.com/pr/42")
        );
        assert_eq!(
            match_bare_url("https://example.com/foo?a=1&b=2), next"),
            Some("https://example.com/foo?a=1&b=2")
        );
        assert_eq!(
            match_bare_url("https://en.wikipedia.org/wiki/Rust_(programming_language)."),
            Some("https://en.wikipedia.org/wiki/Rust_(programming_language)")
        );
        assert_eq!(
            match_bare_url("http://localhost:8787/api/health;"),
            Some("http://localhost:8787/api/health")
        );
        assert_eq!(match_bare_url("https://"), None);
        assert_eq!(match_bare_url("ftp://example.com"), None);
        // Regression: multi-byte UTF-8 char straddling byte index 7/8 after 'h' must not panic.
        assert_eq!(match_bare_url("hello” world"), None);
        assert!(!is_openable_url("hello” world"));
    }

    #[test]
    fn parses_markdown_links_with_balanced_parens_and_scheme_filter() {
        assert_eq!(
            parse_markdown_link("[PR #12](https://github.com/org/repo/pull/12) rest"),
            Some(("PR #12", "https://github.com/org/repo/pull/12", 45))
        );
        assert_eq!(
            parse_markdown_link("[Rust](https://en.wikipedia.org/wiki/Rust_(lang))"),
            Some(("Rust", "https://en.wikipedia.org/wiki/Rust_(lang)", 49))
        );
        assert!(is_openable_url("https://example.com"));
        assert!(is_openable_url("http://127.0.0.1:8787"));
        assert!(!is_openable_url("javascript:alert(1)"));
        assert!(!is_openable_url("file:///etc/passwd"));
    }

    #[test]
    fn clicking_embedded_url_emits_open_url_command() {
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(500.0, 200.0));
        let mut link_center: Option<egui::Pos2> = None;
        let mut plain_pos: Option<egui::Pos2> = None;

        // First frame: measure where the plain text and link glyphs land.
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let text = "See https://example.com/docs for details";
                    let (mut job, links) =
                        build_inline_job(ui, text, 13.5, ui.visuals().text_color(), false);
                    assert_eq!(links.len(), 1);
                    assert_eq!(links[0].url, "https://example.com/docs");
                    job.wrap.max_width = ui.available_width().max(1.0);
                    let galley = ui.fonts(|f| f.layout_job(job));
                    let resp = ui.add(egui::Label::new(Arc::clone(&galley)));
                    let row = &galley.rows[0];
                    if let Some(g) = row
                        .glyphs
                        .iter()
                        .find(|g| g.section_index as usize == links[0].section_index)
                    {
                        link_center = Some(egui::pos2(
                            resp.rect.min.x + g.pos.x + g.advance_width * 0.5,
                            resp.rect.min.y + row.rect.center().y,
                        ));
                    }
                    if let Some(g) = row.glyphs.first() {
                        plain_pos = Some(egui::pos2(
                            resp.rect.min.x + g.pos.x + g.advance_width * 0.5,
                            resp.rect.min.y + row.rect.center().y,
                        ));
                    }
                });
            },
        );

        let click_link = link_center.expect("expected link glyph position");
        let click_plain = plain_pos.expect("expected plain glyph position");

        let click_at = |pos: egui::Pos2| -> Vec<String> {
            let mut opened = Vec::new();
            for pressed in [true, false] {
                let out = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(screen),
                        events: vec![
                            egui::Event::PointerMoved(pos),
                            egui::Event::PointerButton {
                                pos,
                                button: egui::PointerButton::Primary,
                                pressed,
                                modifiers: egui::Modifiers::NONE,
                            },
                        ],
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            render_markdown(ui, "See https://example.com/docs for details");
                        });
                    },
                );
                for cmd in out.platform_output.commands {
                    if let egui::OutputCommand::OpenUrl(open) = cmd {
                        opened.push(open.url);
                    }
                }
            }
            opened
        };

        assert!(
            click_at(click_plain).is_empty(),
            "clicking non-link text should not open a URL"
        );
        assert_eq!(
            click_at(click_link),
            vec!["https://example.com/docs".to_string()]
        );
    }
}
