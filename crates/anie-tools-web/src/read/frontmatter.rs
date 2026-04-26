//! YAML frontmatter emission.
//!
//! Hand-formatted because the field set is small and we want
//! exact control over key ordering and quoting. Avoids
//! pulling in a YAML serialization dep just for this.

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::read::extract::DefuddleOutput;

/// Build a YAML frontmatter block from Defuddle metadata,
/// fenced by `---` lines per the Obsidian / Hugo / Jekyll
/// convention. Fields with no extractable value are omitted
/// rather than emitted as `null` — keeps the output tight.
///
/// `source_url` is the agent-supplied URL; we add it
/// regardless of what Defuddle returned for `domain` so the
/// agent always has the canonical link.
pub fn build(metadata: &DefuddleOutput, source_url: &str) -> String {
    let mut out = String::with_capacity(512);
    out.push_str("---\n");

    if let Some(title) = &metadata.title {
        push_string_field(&mut out, "title", title);
    }
    if let Some(author) = &metadata.author {
        push_string_field(&mut out, "author", author);
    }
    if let Some(published) = &metadata.published {
        push_string_field(&mut out, "published", published);
    }
    if let Some(description) = &metadata.description {
        push_string_field(&mut out, "description", description);
    }
    push_string_field(&mut out, "source", source_url);
    if let Some(site) = metadata.site.as_deref().or(metadata.domain.as_deref()) {
        push_string_field(&mut out, "site", site);
    }
    if let Some(language) = &metadata.language {
        push_string_field(&mut out, "language", language);
    }
    if let Some(word_count) = metadata.word_count {
        out.push_str(&format!("word_count: {word_count}\n"));
    }
    if let Some(reading_time) = metadata.reading_time {
        out.push_str(&format!("reading_time: {reading_time}\n"));
    }
    if let Some(favicon) = &metadata.favicon {
        push_string_field(&mut out, "favicon", favicon);
    }
    let fetched = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string());
    out.push_str(&format!("fetched: {fetched}\n"));
    out.push_str("---\n");
    out
}

/// Emit `key: "value"`. Always quotes the value and escapes
/// embedded quotes / backslashes. YAML is permissive about
/// scalar quoting; we always quote so that titles containing
/// `:` or `#` don't accidentally turn into structure.
fn push_string_field(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push_str(": \"");
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other if (other as u32) < 0x20 => {
                // Other control chars: emit as YAML hex escape.
                out.push_str(&format!("\\x{:02x}", other as u32));
            }
            other => out.push(other),
        }
    }
    out.push_str("\"\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output_with(title: Option<&str>) -> DefuddleOutput {
        DefuddleOutput {
            title: title.map(str::to_string),
            ..DefuddleOutput::default()
        }
    }

    #[test]
    fn frontmatter_starts_and_ends_with_dashes() {
        let yaml = build(&output_with(Some("X")), "https://example.com");
        assert!(yaml.starts_with("---\n"));
        assert!(yaml.ends_with("---\n"));
    }

    #[test]
    fn frontmatter_includes_source_even_if_metadata_empty() {
        let yaml = build(&DefuddleOutput::default(), "https://example.com/x");
        assert!(yaml.contains("source: \"https://example.com/x\""));
    }

    #[test]
    fn frontmatter_omits_fields_without_values() {
        let yaml = build(&DefuddleOutput::default(), "https://example.com");
        assert!(!yaml.contains("title:"));
        assert!(!yaml.contains("author:"));
        assert!(!yaml.contains("description:"));
    }

    #[test]
    fn frontmatter_quotes_strings_with_special_chars() {
        let metadata = DefuddleOutput {
            title: Some("Hello: \"world\"".into()),
            ..DefuddleOutput::default()
        };
        let yaml = build(&metadata, "https://example.com");
        assert!(yaml.contains("title: \"Hello: \\\"world\\\"\""));
    }

    #[test]
    fn frontmatter_escapes_newlines() {
        let metadata = DefuddleOutput {
            description: Some("first line\nsecond line".into()),
            ..DefuddleOutput::default()
        };
        let yaml = build(&metadata, "https://example.com");
        assert!(yaml.contains("description: \"first line\\nsecond line\""));
    }

    #[test]
    fn frontmatter_emits_numeric_fields_unquoted() {
        let metadata = DefuddleOutput {
            word_count: Some(1234),
            reading_time: Some(6),
            ..DefuddleOutput::default()
        };
        let yaml = build(&metadata, "https://example.com");
        assert!(yaml.contains("word_count: 1234"));
        assert!(yaml.contains("reading_time: 6"));
    }

    #[test]
    fn frontmatter_uses_site_falls_back_to_domain() {
        let metadata = DefuddleOutput {
            domain: Some("example.com".into()),
            site: None,
            ..DefuddleOutput::default()
        };
        let yaml = build(&metadata, "https://example.com/x");
        assert!(yaml.contains("site: \"example.com\""));

        let metadata = DefuddleOutput {
            domain: Some("example.com".into()),
            site: Some("Example Org".into()),
            ..DefuddleOutput::default()
        };
        let yaml = build(&metadata, "https://example.com/x");
        assert!(yaml.contains("site: \"Example Org\""));
    }

    #[test]
    fn frontmatter_includes_fetched_timestamp() {
        let yaml = build(&DefuddleOutput::default(), "https://example.com");
        assert!(yaml.contains("fetched: "));
    }
}
