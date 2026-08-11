use crate::CitationStreamParser;
use crate::StreamTextParser;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AssistantTextChunk {
    pub visible_text: String,
    pub citations: Vec<String>,
}

impl AssistantTextChunk {
    pub fn is_empty(&self) -> bool {
        self.visible_text.is_empty() && self.citations.is_empty()
    }
}

/// Parses assistant text streaming markup in one pass:
/// - strips `<oai-mem-citation>` tags and extracts citation payloads
#[derive(Debug, Default)]
pub struct AssistantTextStreamParser {
    citations: CitationStreamParser,
}

impl AssistantTextStreamParser {
    pub fn push_str(&mut self, chunk: &str) -> AssistantTextChunk {
        let citation_chunk = self.citations.push_str(chunk);
        AssistantTextChunk {
            visible_text: citation_chunk.visible_text,
            citations: citation_chunk.extracted,
        }
    }

    pub fn finish(&mut self) -> AssistantTextChunk {
        let citation_chunk = self.citations.finish();
        AssistantTextChunk {
            visible_text: citation_chunk.visible_text,
            citations: citation_chunk.extracted,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AssistantTextStreamParser;
    use pretty_assertions::assert_eq;

    #[test]
    fn parses_citations_across_seed_and_delta_boundaries() {
        let mut parser = AssistantTextStreamParser::default();

        let seeded = parser.push_str("hello <oai-mem-citation>doc");
        let parsed = parser.push_str("1</oai-mem-citation> world");
        let tail = parser.finish();

        assert_eq!(seeded.visible_text, "hello ");
        assert_eq!(seeded.citations, Vec::<String>::new());
        assert_eq!(parsed.visible_text, " world");
        assert_eq!(parsed.citations, vec!["doc1".to_string()]);
        assert_eq!(tail.visible_text, "");
        assert_eq!(tail.citations, Vec::<String>::new());
    }
}
