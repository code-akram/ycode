use std::collections::HashMap;
use std::collections::VecDeque;

use codex_skills::TOOL_MENTION_SIGIL;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LinkedMention {
    pub(crate) sigil: char,
    pub(crate) mention: String,
    pub(crate) path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DecodedHistoryText {
    pub(crate) text: String,
    pub(crate) mentions: Vec<LinkedMention>,
}

#[allow(dead_code)]
pub(crate) fn encode_history_mentions(text: &str, mentions: &[LinkedMention]) -> String {
    if mentions.is_empty() || text.is_empty() {
        return text.to_string();
    }

    let mut mentions_by_name: HashMap<&str, VecDeque<&str>> = HashMap::new();
    for mention in mentions {
        if mention.sigil == TOOL_MENTION_SIGIL && is_skill_path(&mention.path) {
            mentions_by_name
                .entry(mention.mention.as_str())
                .or_default()
                .push_back(mention.path.as_str());
        }
    }

    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == TOOL_MENTION_SIGIL as u8 {
            let name_start = index + 1;
            if let Some(first) = bytes.get(name_start)
                && is_mention_name_char(*first)
            {
                let mut name_end = name_start + 1;
                while let Some(next) = bytes.get(name_end)
                    && is_mention_name_char(*next)
                {
                    name_end += 1;
                }
                let name = &text[name_start..name_end];
                if let Some(path) = mentions_by_name.get_mut(name).and_then(VecDeque::pop_front) {
                    out.push_str(&format!("[${name}]({path})"));
                    index = name_end;
                    continue;
                }
            }
        }

        let Some(ch) = text[index..].chars().next() else {
            break;
        };
        out.push(ch);
        index += ch.len_utf8();
    }
    out
}

pub(crate) fn decode_history_mentions_with_at_mentions(
    text: &str,
    _at_mentions_enabled: bool,
) -> DecodedHistoryText {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut mentions = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'['
            && let Some((name, path, end_index)) = parse_linked_skill_mention(text, bytes, index)
            && !is_common_env_var(name)
            && is_skill_path(path)
        {
            out.push(TOOL_MENTION_SIGIL);
            out.push_str(name);
            mentions.push(LinkedMention {
                sigil: TOOL_MENTION_SIGIL,
                mention: name.to_string(),
                path: path.to_string(),
            });
            index = end_index;
            continue;
        }

        let Some(ch) = text[index..].chars().next() else {
            break;
        };
        out.push(ch);
        index += ch.len_utf8();
    }
    DecodedHistoryText {
        text: out,
        mentions,
    }
}

fn parse_linked_skill_mention<'a>(
    text: &'a str,
    bytes: &[u8],
    start: usize,
) -> Option<(&'a str, &'a str, usize)> {
    let sigil_index = start + 1;
    if bytes.get(sigil_index) != Some(&(TOOL_MENTION_SIGIL as u8)) {
        return None;
    }
    let name_start = sigil_index + 1;
    if !bytes
        .get(name_start)
        .is_some_and(|byte| is_mention_name_char(*byte))
    {
        return None;
    }
    let mut name_end = name_start + 1;
    while bytes
        .get(name_end)
        .is_some_and(|byte| is_mention_name_char(*byte))
    {
        name_end += 1;
    }
    if bytes.get(name_end) != Some(&b']') {
        return None;
    }
    let mut path_start = name_end + 1;
    while bytes.get(path_start).is_some_and(u8::is_ascii_whitespace) {
        path_start += 1;
    }
    if bytes.get(path_start) != Some(&b'(') {
        return None;
    }
    let mut path_end = path_start + 1;
    while bytes.get(path_end).is_some_and(|byte| *byte != b')') {
        path_end += 1;
    }
    if bytes.get(path_end) != Some(&b')') {
        return None;
    }
    let path = text[path_start + 1..path_end].trim();
    (!path.is_empty()).then_some((&text[name_start..name_end], path, path_end + 1))
}

fn is_mention_name_char(byte: u8) -> bool {
    matches!(byte, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-')
}

pub(crate) fn is_common_env_var(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "PATH"
            | "HOME"
            | "USER"
            | "SHELL"
            | "PWD"
            | "TMPDIR"
            | "TEMP"
            | "TMP"
            | "LANG"
            | "TERM"
            | "XDG_CONFIG_HOME"
    )
}

fn is_skill_path(path: &str) -> bool {
    path.starts_with("skill://")
        || path
            .rsplit(['/', '\\'])
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case("SKILL.md"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn history_round_trip_preserves_filesystem_skill_mentions() {
        let mention = LinkedMention {
            sigil: '$',
            mention: "sample".to_string(),
            path: "/tmp/sample/SKILL.md".to_string(),
        };
        let encoded = encode_history_mentions("use $sample", std::slice::from_ref(&mention));
        assert_eq!(encoded, "use [$sample](/tmp/sample/SKILL.md)");
        assert_eq!(
            decode_history_mentions_with_at_mentions(&encoded, true),
            DecodedHistoryText {
                text: "use $sample".to_string(),
                mentions: vec![mention],
            }
        );
    }
}
