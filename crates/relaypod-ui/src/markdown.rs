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
            let job = build_inline_job(ui, content, size, color, true);
            ui.add(egui::Label::new(job).wrap());
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
        let job = build_inline_job(ui, line, 13.5, color, false);
        ui.add(egui::Label::new(job).wrap());
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
                let job = build_inline_job(ui, line, 13.0, color, false);
                ui.add(egui::Label::new(job).wrap());
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
        let job = build_inline_job(ui, content, 13.5, color, false);
        ui.add(egui::Label::new(job).wrap());
    });
}

fn build_inline_job(
    ui: &egui::Ui,
    text: &str,
    base_size: f32,
    base_color: Color32,
    strong_default: bool,
) -> LayoutJob {
    let mut job = LayoutJob::default();
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
            if let Some(close_bracket) = text[i + 1..].find("](") {
                let label_start = i + 1;
                let label_end = label_start + close_bracket;
                let url_start = label_end + 2;
                if let Some(close_paren) = text[url_start..].find(')') {
                    flush_plain(&mut job, plain_start, i);
                    let label = &text[label_start..label_end];
                    job.append(
                        label,
                        0.0,
                        TextFormat {
                            font_id: FontId::new(base_size, FontFamily::Proportional),
                            color: link_fg,
                            underline: Stroke::new(1.0_f32, link_fg),
                            ..Default::default()
                        },
                    );
                    i = url_start + close_paren + 1;
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
    job
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
}
