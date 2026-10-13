//! Context is always assembled in the same order, so provider prompt caches
//! keep hitting: static prefix (charter, SOUL, tool definitions), then the
//! memory snapshot, then the day's transcript, which ends with the wake
//! message (ARCHITECTURE.md, "The main thread").

use harness_providers::{Message, ModelRequest, ToolSpec};

#[derive(Debug, Clone)]
pub struct ContextAssembler {
    /// The capability profile the request is for.
    pub profile: String,
    /// Charter and SOUL. Fixed for the whole day.
    pub static_prefix: String,
    /// `MEMORY.md`, `USER.md` and the handoff note, as of the start of the day.
    pub memory_snapshot: String,
    pub max_output_tokens: u32,
}

impl ContextAssembler {
    pub fn assemble(&self, tools: Vec<ToolSpec>, transcript: &[Message]) -> ModelRequest {
        let mut messages = Vec::with_capacity(transcript.len() + 1);
        messages.push(Message::user_text(format!(
            "<memory>\n{}\n</memory>",
            self.memory_snapshot
        )));
        messages.extend_from_slice(transcript);

        // One breakpoint after the memory snapshot, which is stable all day,
        // and one at the end of everything before the newest message, so the
        // next turn reuses this one's prefix.
        let mut cache_breakpoints = vec![0];
        if messages.len() > 2 {
            cache_breakpoints.push(messages.len() - 2);
        }

        ModelRequest {
            profile: self.profile.clone(),
            system: self.static_prefix.clone(),
            tools,
            messages,
            cache_breakpoints,
            max_output_tokens: self.max_output_tokens,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assembler() -> ContextAssembler {
        ContextAssembler {
            profile: "main".into(),
            static_prefix: "charter + SOUL".into(),
            memory_snapshot: "MEMORY.md".into(),
            max_output_tokens: 4096,
        }
    }

    #[test]
    fn order_is_prefix_memory_transcript() {
        let transcript = vec![
            Message::user_text("wake 1"),
            Message::assistant_text("thinking"),
            Message::user_text("wake 2"),
        ];
        let req = assembler().assemble(vec![], &transcript);
        assert_eq!(req.system, "charter + SOUL");
        assert_eq!(req.messages.len(), 4);
        assert!(matches!(
            &req.messages[0].content[0],
            harness_providers::ContentBlock::Text { text } if text.contains("MEMORY.md")
        ));
        assert_eq!(&req.messages[1..], &transcript[..]);
        assert_eq!(req.cache_breakpoints, vec![0, 2]);
    }

    #[test]
    fn earlier_requests_are_prefixes_of_later_ones() {
        let a = assembler();
        let mut transcript = vec![Message::user_text("wake 1")];
        let first = a.assemble(vec![], &transcript);
        transcript.push(Message::assistant_text("ok"));
        transcript.push(Message::user_text("wake 2"));
        let second = a.assemble(vec![], &transcript);
        assert_eq!(first.system, second.system);
        assert_eq!(
            &second.messages[..first.messages.len()],
            &first.messages[..]
        );
    }
}
