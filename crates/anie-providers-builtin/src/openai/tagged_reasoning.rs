//! Tag-based reasoning extraction for OpenAI-compatible streams.
//!
//! Some local models (Qwen, DeepSeek, etc.) emit reasoning inline in
//! the text stream wrapped in `<think>`, `<thinking>`, or
//! `<reasoning>` XML-ish tags. `TaggedReasoningSplitter` extracts
//! those regions as `StreamContentPart::Thinking`, leaving the rest
//! as `StreamContentPart::Text`. Tag boundaries may span arbitrary
//! chunk arrivals; the splitter buffers partial tags until it can
//! disambiguate.

const TAGGED_REASONING_TAGS: [(&str, &str); 3] = [
    ("<think>", "</think>"),
    ("<thinking>", "</thinking>"),
    ("<reasoning>", "</reasoning>"),
];

pub(super) enum StreamContentPart {
    Text(String),
    Thinking(String),
}

#[derive(Clone, Copy)]
enum TaggedReasoningMode {
    Text,
    Thinking { closing_tag: &'static str },
}

pub(super) struct TaggedReasoningSplitter {
    mode: TaggedReasoningMode,
    pending: String,
}

impl Default for TaggedReasoningSplitter {
    fn default() -> Self {
        Self {
            mode: TaggedReasoningMode::Text,
            pending: String::new(),
        }
    }
}

impl TaggedReasoningSplitter {
    pub(super) fn push(&mut self, fragment: &str) -> Vec<StreamContentPart> {
        self.pending.push_str(fragment);
        self.drain(false)
    }

    pub(super) fn finish(&mut self) -> Vec<StreamContentPart> {
        self.drain(true)
    }

    fn drain(&mut self, finish: bool) -> Vec<StreamContentPart> {
        let mut parts = Vec::new();

        loop {
            match self.mode {
                TaggedReasoningMode::Text => {
                    if self.pending.is_empty() {
                        break;
                    }

                    if let Some(open_index) = self.pending.find('<') {
                        if open_index > 0 {
                            // Plan 06 PR-D: move-based split.
                            // `split_off` hands back the
                            // [open_index..] suffix; `mem::replace`
                            // swaps it into `self.pending`, leaving
                            // the [..open_index] prefix as an owned
                            // String without the char-by-char
                            // `drain().collect()` iteration.
                            let tail = self.pending.split_off(open_index);
                            let text = std::mem::replace(&mut self.pending, tail);
                            Self::push_part(&mut parts, StreamContentPart::Text(text));
                            continue;
                        }

                        if let Some((open_tag, closing_tag)) =
                            tagged_reasoning_open_tag(&self.pending)
                        {
                            self.pending.drain(..open_tag.len());
                            self.mode = TaggedReasoningMode::Thinking { closing_tag };
                            continue;
                        }

                        if !finish && is_prefix_of_any_open_tag(&self.pending) {
                            break;
                        }

                        let text = drain_first_char(&mut self.pending);
                        Self::push_part(&mut parts, StreamContentPart::Text(text));
                        continue;
                    }

                    let text = std::mem::take(&mut self.pending);
                    Self::push_part(&mut parts, StreamContentPart::Text(text));
                    break;
                }
                TaggedReasoningMode::Thinking { closing_tag } => {
                    if self.pending.is_empty() {
                        break;
                    }

                    if let Some(close_index) = self.pending.find('<') {
                        if close_index > 0 {
                            // Plan 06 PR-D: move-based split (see
                            // Text mode above).
                            let tail = self.pending.split_off(close_index);
                            let thinking =
                                std::mem::replace(&mut self.pending, tail);
                            Self::push_part(&mut parts, StreamContentPart::Thinking(thinking));
                            continue;
                        }

                        if self.pending.starts_with(closing_tag) {
                            self.pending.drain(..closing_tag.len());
                            self.mode = TaggedReasoningMode::Text;
                            continue;
                        }

                        if !finish && closing_tag.starts_with(&self.pending) {
                            break;
                        }

                        let thinking = drain_first_char(&mut self.pending);
                        Self::push_part(&mut parts, StreamContentPart::Thinking(thinking));
                        continue;
                    }

                    let thinking = std::mem::take(&mut self.pending);
                    Self::push_part(&mut parts, StreamContentPart::Thinking(thinking));
                    break;
                }
            }
        }

        parts
    }

    fn push_part(parts: &mut Vec<StreamContentPart>, part: StreamContentPart) {
        match part {
            StreamContentPart::Text(text) if text.is_empty() => {}
            StreamContentPart::Thinking(thinking) if thinking.is_empty() => {}
            StreamContentPart::Text(text) => match parts.last_mut() {
                Some(StreamContentPart::Text(existing)) => existing.push_str(&text),
                _ => parts.push(StreamContentPart::Text(text)),
            },
            StreamContentPart::Thinking(thinking) => match parts.last_mut() {
                Some(StreamContentPart::Thinking(existing)) => existing.push_str(&thinking),
                _ => parts.push(StreamContentPart::Thinking(thinking)),
            },
        }
    }
}

fn tagged_reasoning_open_tag(input: &str) -> Option<(&'static str, &'static str)> {
    TAGGED_REASONING_TAGS
        .iter()
        .find_map(|(open_tag, closing_tag)| {
            input
                .starts_with(open_tag)
                .then_some((*open_tag, *closing_tag))
        })
}

fn is_prefix_of_any_open_tag(input: &str) -> bool {
    TAGGED_REASONING_TAGS
        .iter()
        .any(|(open_tag, _)| open_tag.starts_with(input))
}

fn drain_first_char(input: &mut String) -> String {
    let first_char_len = input.chars().next().map_or(0, char::len_utf8);
    input.drain(..first_char_len).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts_to_pairs(parts: Vec<StreamContentPart>) -> Vec<(&'static str, String)> {
        parts
            .into_iter()
            .map(|p| match p {
                StreamContentPart::Text(t) => ("text", t),
                StreamContentPart::Thinking(t) => ("thinking", t),
            })
            .collect()
    }

    fn drive(inputs: &[&str], finish: bool) -> Vec<(&'static str, String)> {
        let mut splitter = TaggedReasoningSplitter::default();
        let mut all = Vec::new();
        for chunk in inputs {
            all.extend(splitter.push(chunk));
        }
        if finish {
            all.extend(splitter.finish());
        }
        parts_to_pairs(all)
    }

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(
            drive(&["hello world"], true),
            vec![("text", "hello world".to_string())]
        );
    }

    #[test]
    fn think_tag_emits_thinking() {
        assert_eq!(
            drive(&["<think>foo</think>bar"], true),
            vec![("thinking", "foo".to_string()), ("text", "bar".to_string()),]
        );
    }

    #[test]
    fn thinking_tag_emits_thinking() {
        assert_eq!(
            drive(&["<thinking>foo</thinking>"], true),
            vec![("thinking", "foo".to_string())]
        );
    }

    #[test]
    fn reasoning_tag_emits_thinking() {
        assert_eq!(
            drive(&["<reasoning>foo</reasoning>"], true),
            vec![("thinking", "foo".to_string())]
        );
    }

    #[test]
    fn tag_split_across_chunks() {
        assert_eq!(
            drive(&["<thi", "nk>foo</think>"], true),
            vec![("thinking", "foo".to_string())]
        );
    }

    #[test]
    fn partial_open_tag_at_end_is_buffered() {
        let mut splitter = TaggedReasoningSplitter::default();
        let p1 = splitter.push("hello <thi");
        assert_eq!(
            parts_to_pairs(p1),
            vec![("text", "hello ".to_string())],
            "text before partial tag should emit; partial tag buffered"
        );
        let p2 = splitter.push("nk>x</think>");
        let p2_pairs = parts_to_pairs(p2);
        assert_eq!(
            p2_pairs,
            vec![("thinking", "x".to_string())],
            "resolved tag emits thinking block"
        );
    }

    #[test]
    fn unterminated_open_tag_on_finish_flushes_as_text() {
        // Feeding `<think>` without a close and then finishing: once
        // the open tag is recognized, we commit to Thinking mode.
        // Any buffered text after it flushes as thinking on finish.
        let result = drive(&["<think>oops"], true);
        assert_eq!(result, vec![("thinking", "oops".to_string())]);
    }

    #[test]
    fn lone_less_than_at_finish_flushes_as_text() {
        // A bare `<` that never resolves into a known open tag is
        // flushed as literal text on finish.
        let result = drive(&["a < b"], true);
        assert_eq!(result, vec![("text", "a < b".to_string())]);
    }

    #[test]
    fn nested_tags_are_treated_as_text_in_thinking_mode() {
        // Inside a Thinking span, a second `<think>` is not
        // recognized; it's treated as thinking text. The first
        // `</think>` closes the outer block; the remaining `</think>`
        // becomes text. This characterization pins current behavior.
        let result = drive(&["<think>outer<think>inner</think></think>"], true);
        assert_eq!(
            result,
            vec![
                ("thinking", "outer<think>inner".to_string()),
                ("text", "</think>".to_string()),
            ]
        );
    }

    #[test]
    fn utf8_boundary_inside_tag_name() {
        // Feed a partial open tag, then complete it with multibyte
        // payload. The emoji must survive intact.
        let mut splitter = TaggedReasoningSplitter::default();
        let p1 = splitter.push("<thin");
        assert!(parts_to_pairs(p1).is_empty(), "partial tag buffers");
        let p2 = splitter.push("k>😀</think>");
        assert_eq!(parts_to_pairs(p2), vec![("thinking", "😀".to_string())]);
    }

    #[test]
    fn multiple_thinking_segments() {
        assert_eq!(
            drive(&["<think>a</think>b<think>c</think>"], true),
            vec![
                ("thinking", "a".to_string()),
                ("text", "b".to_string()),
                ("thinking", "c".to_string()),
            ]
        );
    }

    #[test]
    fn empty_thinking_block_is_omitted() {
        // `push_part` drops empty text and empty thinking. Pins
        // current behavior.
        let result = drive(&["<think></think>after"], true);
        assert_eq!(result, vec![("text", "after".to_string())]);
    }

    #[test]
    fn case_sensitive_tags() {
        // Upper-case `<THINK>` is not recognized — current behavior.
        let result = drive(&["<THINK>not recognized</THINK>"], true);
        assert_eq!(
            result,
            vec![("text", "<THINK>not recognized</THINK>".to_string())]
        );
    }
}
