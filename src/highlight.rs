//! Small display-only lexical approximation, intentionally not a language parser.
use eframe::egui::{self, Color32, FontId, TextFormat};
use std::path::Path;

pub fn supports_path(path: &Path) -> bool {
    path.extension()
        .and_then(|x| x.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "rs" | "js"
                    | "jsx"
                    | "ts"
                    | "tsx"
                    | "json"
                    | "py"
                    | "c"
                    | "h"
                    | "cpp"
                    | "hpp"
                    | "cs"
                    | "java"
                    | "go"
                    | "toml"
                    | "yaml"
                    | "yml"
                    | "sh"
                    | "css"
            )
        })
}

pub fn layout(
    text: &str,
    font_id: &FontId,
    dark: bool,
    enabled: bool,
    wrap_width: f32,
) -> egui::text::LayoutJob {
    let base = if dark {
        Color32::from_rgb(224, 226, 231)
    } else {
        Color32::from_rgb(34, 37, 43)
    };
    let keyword = if dark {
        Color32::from_rgb(173, 157, 245)
    } else {
        Color32::from_rgb(107, 53, 166)
    };
    let string = if dark {
        Color32::from_rgb(184, 208, 146)
    } else {
        Color32::from_rgb(49, 111, 63)
    };
    let comment = if dark {
        Color32::from_rgb(133, 151, 157)
    } else {
        Color32::from_rgb(105, 117, 120)
    };
    let number = if dark {
        Color32::from_rgb(226, 185, 132)
    } else {
        Color32::from_rgb(158, 92, 32)
    };
    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = wrap_width;
    let mut append = |slice: &str, color| {
        job.append(
            slice,
            0.0,
            TextFormat {
                font_id: font_id.clone(),
                color,
                ..Default::default()
            },
        )
    };
    if !enabled {
        append(text, base);
        return job;
    }
    let mut at = 0;
    while at < text.len() {
        let remaining = &text[at..];
        let ch = remaining.chars().next().unwrap();
        let (len, color) = if remaining.starts_with("//") || ch == '#' {
            (remaining.find('\n').unwrap_or(remaining.len()), comment)
        } else if let Some(stripped) = remaining.strip_prefix("/*") {
            (
                stripped
                    .find("*/")
                    .map(|x| x + 4)
                    .unwrap_or(remaining.len()),
                comment,
            )
        } else if ch == '"' || ch == '\'' {
            let mut end = ch.len_utf8();
            let mut escaped = false;
            for c in remaining[ch.len_utf8()..].chars() {
                end += c.len_utf8();
                if escaped {
                    escaped = false;
                    continue;
                }
                if c == '\\' {
                    escaped = true;
                } else if c == ch {
                    break;
                }
            }
            (end, string)
        } else if ch.is_ascii_digit() {
            (
                remaining
                    .char_indices()
                    .find(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '.' || *c == '_'))
                    .map(|(i, _)| i)
                    .unwrap_or(remaining.len()),
                number,
            )
        } else if ch.is_alphabetic() || ch == '_' {
            let len = remaining
                .char_indices()
                .find(|(_, c)| !(c.is_alphanumeric() || *c == '_'))
                .map(|(i, _)| i)
                .unwrap_or(remaining.len());
            let word = &remaining[..len];
            let special = matches!(
                word,
                "fn" | "let"
                    | "mut"
                    | "pub"
                    | "use"
                    | "mod"
                    | "impl"
                    | "struct"
                    | "enum"
                    | "trait"
                    | "match"
                    | "if"
                    | "else"
                    | "for"
                    | "while"
                    | "loop"
                    | "return"
                    | "break"
                    | "continue"
                    | "const"
                    | "static"
                    | "true"
                    | "false"
                    | "null"
                    | "None"
                    | "Some"
                    | "self"
                    | "Self"
                    | "async"
                    | "await"
                    | "function"
                    | "class"
                    | "import"
                    | "export"
                    | "from"
                    | "def"
                    | "in"
                    | "as"
                    | "try"
                    | "catch"
                    | "throw"
                    | "new"
                    | "var"
                    | "public"
                    | "private"
                    | "void"
                    | "int"
                    | "bool"
                    | "package"
            );
            (len, if special { keyword } else { base })
        } else {
            (ch.len_utf8(), base)
        };
        append(&text[at..at + len], color);
        at += len;
    }
    job
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_remains_exact_including_unicode() {
        let source = "fn main() { let café = \"你好\\\"🌍\"; // words\n /* comment */ }\n";
        let result = layout(source, &FontId::monospace(14.0), true, true, 640.0);
        assert_eq!(result.text, source);
        assert!(result.sections.len() > 3);
        for section in result.sections {
            assert!(source.is_char_boundary(section.byte_range.start.into()));
            assert!(source.is_char_boundary(section.byte_range.end.into()));
        }
    }
    #[test]
    fn plain_text_and_empty_input() {
        assert_eq!(layout("", &FontId::default(), false, true, 400.0).text, "");
        let result = layout("plain 123", &FontId::default(), false, false, f32::INFINITY);
        assert_eq!(result.text, "plain 123");
        assert_eq!(result.sections.len(), 1);
        assert!(!supports_path(Path::new("notes.txt")));
        assert!(supports_path(Path::new("source.RS")));
    }
}
