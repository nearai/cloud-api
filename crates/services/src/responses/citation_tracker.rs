//! Citation tracking with state machine for parsing [s:N] tags during LLM streaming
//!
//! This module handles parsing source citations from LLM responses using a state machine
//! that properly handles tags split across tokens. The tracker processes tokens incrementally,
//! removing tags and emitting clean text while tracking citation positions in real-time.

/// Citation tag state machine states
#[derive(Debug, Clone, Copy, PartialEq)]
enum TagState {
    /// No tag in progress, accumulating normal text
    Idle,

    /// Saw '[', waiting for 's' or '/'
    PartialOpen,

    /// Saw '[s', waiting for ':'
    PartialOpenTag,

    /// Saw '[s:', accumulating digits (waiting for ] or more digits)
    PartialOpenTagColon,

    /// Saw '[s:N+', waiting for ']'
    OpenTagDigit,

    /// Saw '[/', waiting for 's'
    PartialCloseTag,

    /// Saw '[/s', waiting for ':'
    PartialCloseTagS,

    /// Saw '[/s:', accumulating digits (waiting for ] or more digits)
    PartialCloseTagColon,

    /// Saw '[/s:N+', waiting for ']'
    CloseTagDigit,

    /// Inside a citation [s:N]...[/s:N]
    InsideTag { source_id: usize },
}

/// Active citation being accumulated during streaming
#[derive(Debug, Clone)]
struct ActiveCitation {
    source_id: usize,
    start_index: usize,
    accumulated_content: String,
}

/// Completed citation with source mapping
#[derive(Debug, Clone)]
pub struct Citation {
    pub start_index: usize,
    pub end_index: usize,
    pub source_id: usize,
    pub cited_text: String,
}

/// Citation tracker using state machine for robust tag parsing
///
/// Processes tokens incrementally, removing citation tags and tracking positions in real-time.
/// When a citation closes, it's immediately added to completed_citations with correct indices.
#[derive(Debug, Clone)]
pub struct CitationTracker {
    /// Clean accumulated text (tags removed)
    clean_text: String,

    /// Current parsing state
    current_state: TagState,

    /// Buffer for incomplete tokens (may contain partial tags)
    token_buffer: String,

    /// Current character position in clean_text (incremented when clean chars added)
    clean_position: usize,

    /// Active citation being accumulated
    active_citation: Option<ActiveCitation>,

    /// Completed citations with correct indices (populated when citations close)
    completed_citations: Vec<Citation>,

    /// Previous state (for context when recovering from failed tags)
    previous_state: Option<TagState>,
}

impl Default for CitationTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl CitationTracker {
    pub fn new() -> Self {
        Self {
            clean_text: String::new(),
            current_state: TagState::Idle,
            token_buffer: String::new(),
            clean_position: 0,
            active_citation: None,
            completed_citations: Vec::new(),
            previous_state: None,
        }
    }

    /// Add a token from the LLM stream and return clean output (with tags removed)
    /// The returned String is the clean text portion to send via SSE
    pub fn add_token(&mut self, token: &str) -> String {
        let mut output = String::new();
        for ch in token.chars() {
            if let Some(clean_ch) = self.process_char(ch) {
                output.push(clean_ch);
            }
        }
        output
    }

    /// Process a single character through the state machine
    /// Returns Some(ch) if a clean character should be output, None if it's part of a tag
    fn process_char(&mut self, ch: char) -> Option<char> {
        match self.current_state {
            TagState::Idle => {
                if ch == '[' {
                    // Start of potential tag
                    self.token_buffer.push(ch);
                    self.previous_state = Some(TagState::Idle);
                    self.current_state = TagState::PartialOpen;
                    None // Don't output yet, wait for next char
                } else {
                    // Regular character - add to clean_text and output
                    self.clean_text.push(ch);
                    if let Some(ref mut active) = self.active_citation {
                        active.accumulated_content.push(ch);
                    }
                    self.clean_position += 1;
                    Some(ch)
                }
            }

            TagState::PartialOpen => {
                if ch == 's' {
                    self.token_buffer.push(ch);
                    // Might be [s:N]
                    self.current_state = TagState::PartialOpenTag;
                    None // Still buffering
                } else if ch == '/' {
                    self.token_buffer.push(ch);
                    // Might be [/s:N]
                    self.current_state = TagState::PartialCloseTag;
                    None // Still buffering
                } else {
                    // Not a tag, flush buffer as literal text
                    self.token_buffer.push(ch);
                    self.flush_token_buffer_and_restore_state()
                }
            }

            TagState::PartialOpenTag => {
                self.token_buffer.push(ch);
                if ch == ':' {
                    self.current_state = TagState::PartialOpenTagColon;
                    None // Still buffering
                } else {
                    // Invalid tag, flush as literal
                    self.flush_token_buffer_and_restore_state()
                }
            }

            TagState::PartialOpenTagColon => {
                self.token_buffer.push(ch);
                if ch.is_ascii_digit() {
                    // Accumulate digit, still waiting for ] or more digits
                    self.current_state = TagState::OpenTagDigit;
                    None
                } else {
                    // Invalid, flush as literal
                    self.flush_token_buffer_and_restore_state()
                }
            }

            TagState::OpenTagDigit => {
                self.token_buffer.push(ch);
                if ch == ']' {
                    // Complete opening tag [s:N+]
                    // Extract digits from token_buffer: "[s:" + digits + "]"
                    let digits_part = &self.token_buffer[3..self.token_buffer.len() - 1];
                    if let Ok(source_id) = digits_part.parse::<usize>() {
                        tracing::debug!(
                            "CitationTracker: Citation tag opened [s:{}] at clean_position={}",
                            source_id,
                            self.clean_position
                        );
                        self.current_state = TagState::InsideTag { source_id };

                        // Start new citation at current clean_position
                        self.active_citation = Some(ActiveCitation {
                            source_id,
                            start_index: self.clean_position,
                            accumulated_content: String::new(),
                        });

                        self.token_buffer.clear();
                        self.previous_state = None;
                        None // Tag consumed, don't output
                    } else {
                        // Failed to parse source_id, flush as literal
                        self.flush_token_buffer_and_restore_state()
                    }
                } else if !ch.is_ascii_digit() {
                    // Invalid (expected ] or another digit), flush as literal
                    self.flush_token_buffer_and_restore_state()
                } else {
                    // Another digit, keep buffering
                    None
                }
            }

            TagState::PartialCloseTag => {
                if ch == 's' {
                    self.token_buffer.push(ch);
                    // [/s found, waiting for :
                    self.current_state = TagState::PartialCloseTagS;
                    None
                } else {
                    // Invalid, flush as literal
                    self.token_buffer.push(ch);
                    self.flush_token_buffer_and_restore_state()
                }
            }

            TagState::PartialCloseTagS => {
                if ch == ':' {
                    self.token_buffer.push(ch);
                    self.current_state = TagState::PartialCloseTagColon;
                    None
                } else {
                    // Invalid, flush as literal
                    self.token_buffer.push(ch);
                    self.flush_token_buffer_and_restore_state()
                }
            }

            TagState::PartialCloseTagColon => {
                self.token_buffer.push(ch);
                if ch.is_ascii_digit() {
                    // Accumulate digit, still waiting for ] or more digits
                    self.current_state = TagState::CloseTagDigit;
                    None
                } else {
                    // Invalid, flush as literal
                    self.flush_token_buffer_and_restore_state()
                }
            }

            TagState::CloseTagDigit => {
                self.token_buffer.push(ch);
                if ch == ']' {
                    // Complete closing tag [/s:N+]
                    // Extract digits from token_buffer: "[/s:" + digits + "]"
                    let digits_part = &self.token_buffer[4..self.token_buffer.len() - 1];
                    if let Ok(source_id) = digits_part.parse::<usize>() {
                        // Citation is closing - finalize it immediately with correct indices
                        if let Some(active) = self.active_citation.take() {
                            if active.source_id == source_id {
                                tracing::debug!("CitationTracker: Citation tag closed [/s:{}] - indices=[{}, {}], text='{}'", source_id, active.start_index, self.clean_position, active.accumulated_content);
                                self.completed_citations.push(Citation {
                                    start_index: active.start_index,
                                    end_index: self.clean_position,
                                    source_id,
                                    cited_text: active.accumulated_content,
                                });
                            }
                        }

                        self.current_state = TagState::Idle;
                        self.token_buffer.clear();
                        self.previous_state = None;
                        None // Tag consumed, don't output
                    } else {
                        // Failed to parse source_id, flush as literal
                        self.flush_token_buffer_and_restore_state()
                    }
                } else if !ch.is_ascii_digit() {
                    // Invalid (expected ] or another digit), flush as literal
                    self.flush_token_buffer_and_restore_state()
                } else {
                    // Another digit, keep buffering
                    None
                }
            }

            TagState::InsideTag { source_id } => {
                if ch == '[' {
                    // Might be start of closing tag, begin buffering
                    self.token_buffer.push(ch);
                    self.previous_state = Some(TagState::InsideTag { source_id });
                    self.current_state = TagState::PartialOpen;
                    None // Wait for next char before outputting
                } else {
                    // Regular character inside citation - output and track
                    self.clean_text.push(ch);
                    if let Some(ref mut active) = self.active_citation {
                        active.accumulated_content.push(ch);
                    }
                    self.clean_position += 1;
                    Some(ch)
                }
            }
        }
    }

    /// Flush token buffer and restore previous state, outputting each char
    fn flush_token_buffer_and_restore_state(&mut self) -> Option<char> {
        let mut last_char = None;
        for ch in self.token_buffer.chars() {
            self.clean_text.push(ch);
            if let Some(ref mut active) = self.active_citation {
                active.accumulated_content.push(ch);
            }
            self.clean_position += 1;
            last_char = Some(ch);
        }
        self.token_buffer.clear();
        self.current_state = self.previous_state.take().unwrap_or(TagState::Idle);
        last_char // Return the last character from the flushed buffer
    }

    /// Finalize tracking and return clean text with citations
    /// Any incomplete tags at the end are treated as literal text
    pub fn finalize(mut self) -> (String, Vec<Citation>) {
        // If there's pending token_buffer (incomplete tag at end), treat as literal
        if !self.token_buffer.is_empty() {
            tracing::debug!(
                "CitationTracker: Flushing incomplete token_buffer at finalize: '{}'",
                self.token_buffer
            );
            for ch in self.token_buffer.chars() {
                self.clean_text.push(ch);
                if let Some(ref mut active) = self.active_citation {
                    active.accumulated_content.push(ch);
                }
            }
        }

        tracing::debug!(
            "CitationTracker: Finalizing with {} completed citations",
            self.completed_citations.len()
        );
        for (idx, citation) in self.completed_citations.iter().enumerate() {
            tracing::debug!(
                "CitationTracker: Citation {}: source_id={}, indices=[{}, {}]",
                idx,
                citation.source_id,
                citation.start_index,
                citation.end_index
            );
        }

        // Return clean text and citations (already have correct indices from real-time tracking)
        (self.clean_text, self.completed_citations)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_citation() {
        let mut tracker = CitationTracker::new();
        let out1 = tracker.add_token("Hello ");
        let out2 = tracker.add_token("[s:0]");
        let out3 = tracker.add_token("world");
        let out4 = tracker.add_token("[/s:0]");

        // Verify incremental output removes tags immediately
        assert_eq!(out1, "Hello ");
        assert_eq!(out2, ""); // Opening tag is consumed
        assert_eq!(out3, "world");
        assert_eq!(out4, ""); // Closing tag is consumed

        let (clean, citations) = tracker.finalize();
        assert_eq!(clean, "Hello world");
        assert_eq!(citations.len(), 1);
        assert_eq!(citations[0].source_id, 0);
        assert_eq!(citations[0].start_index, 6);
        assert_eq!(citations[0].end_index, 11);
    }

    #[test]
    fn test_split_tag_across_tokens() {
        let mut tracker = CitationTracker::new();
        let out1 = tracker.add_token("Hello ");
        let out2 = tracker.add_token("[s"); // Split: only "["
        let out3 = tracker.add_token(":0]"); // Split: ":0]"
        let out4 = tracker.add_token("world");
        let out5 = tracker.add_token("[/s:0]");

        // Verify that split tags are handled correctly
        assert_eq!(out1, "Hello ");
        assert_eq!(out2, ""); // Buffering partial tag
        assert_eq!(out3, ""); // Closing partial tag
        assert_eq!(out4, "world");
        assert_eq!(out5, ""); // Closing tag consumed

        let (clean, citations) = tracker.finalize();
        assert_eq!(clean, "Hello world");
        assert_eq!(citations.len(), 1);
        assert_eq!(citations[0].start_index, 6);
        assert_eq!(citations[0].end_index, 11);
    }

    #[test]
    fn test_tag_split_at_digit() {
        let mut tracker = CitationTracker::new();
        tracker.add_token("[s:");
        tracker.add_token("0]");
        tracker.add_token("text");
        tracker.add_token("[/s:0]");

        let (clean, citations) = tracker.finalize();
        assert_eq!(clean, "text");
        assert_eq!(citations.len(), 1);
    }

    #[test]
    fn test_multiple_citations() {
        let mut tracker = CitationTracker::new();
        tracker.add_token("Text [s:0]cited1[/s:0] and [s:1]cited2[/s:1]");

        let (clean, citations) = tracker.finalize();
        assert_eq!(clean, "Text cited1 and cited2");
        assert_eq!(citations.len(), 2);
        assert_eq!(citations[0].source_id, 0);
        assert_eq!(citations[1].source_id, 1);
    }

    #[test]
    fn test_incomplete_tag_at_end() {
        let mut tracker = CitationTracker::new();
        tracker.add_token("Text [s");

        let (clean, citations) = tracker.finalize();
        assert_eq!(clean, "Text [s");
        assert_eq!(citations.len(), 0);
    }

    #[test]
    fn test_invalid_tag_flushed() {
        let mut tracker = CitationTracker::new();
        tracker.add_token("Text [x:0]content[/x:0]");

        let (clean, citations) = tracker.finalize();
        assert_eq!(clean, "Text [x:0]content[/x:0]");
        assert_eq!(citations.len(), 0);
    }

    #[test]
    fn test_no_citations() {
        let mut tracker = CitationTracker::new();
        tracker.add_token("Just plain text");

        let (clean, citations) = tracker.finalize();
        assert_eq!(clean, "Just plain text");
        assert_eq!(citations.len(), 0);
    }

    #[test]
    fn test_multi_digit_source_ids() {
        let mut tracker = CitationTracker::new();
        tracker.add_token("Hello ");
        tracker.add_token("[s:10]");
        tracker.add_token("world");
        tracker.add_token("[/s:10]");

        let (clean, citations) = tracker.finalize();
        assert_eq!(clean, "Hello world");
        assert_eq!(citations.len(), 1);
        assert_eq!(citations[0].source_id, 10);
        assert_eq!(citations[0].start_index, 6);
        assert_eq!(citations[0].end_index, 11);
        assert_eq!(citations[0].cited_text, "world");
    }

    #[test]
    fn test_triple_digit_source_ids() {
        let mut tracker = CitationTracker::new();
        tracker.add_token("Info ");
        tracker.add_token("[s:999]");
        tracker.add_token("here");
        tracker.add_token("[/s:999]");

        let (clean, citations) = tracker.finalize();
        assert_eq!(clean, "Info here");
        assert_eq!(citations.len(), 1);
        assert_eq!(citations[0].source_id, 999);
    }

    #[test]
    fn test_mixed_digit_counts() {
        let mut tracker = CitationTracker::new();
        tracker.add_token("[s:0]one[/s:0] [s:10]two[/s:10] [s:100]three[/s:100]");

        let (clean, citations) = tracker.finalize();
        assert_eq!(clean, "one two three");
        assert_eq!(citations.len(), 3);
        assert_eq!(citations[0].source_id, 0);
        assert_eq!(citations[1].source_id, 10);
        assert_eq!(citations[2].source_id, 100);
    }

    #[test]
    fn test_streaming_incremental_output() {
        // Test that verifies the streaming behavior: clean text is returned immediately
        let mut tracker = CitationTracker::new();
        let mut accumulated = String::new();

        let out = tracker.add_token("[s:0]");
        accumulated.push_str(&out);
        assert_eq!(accumulated, ""); // Opening tag consumed

        let out = tracker.add_token("cited");
        accumulated.push_str(&out);
        assert_eq!(accumulated, "cited"); // Cited text output

        let out = tracker.add_token("[/s:0] more text");
        accumulated.push_str(&out);
        assert_eq!(accumulated, "cited more text"); // Closing tag consumed, rest output

        let (clean, citations) = tracker.finalize();
        assert_eq!(clean, "cited more text");
        assert_eq!(citations.len(), 1);
        assert_eq!(citations[0].start_index, 0);
        assert_eq!(citations[0].end_index, 5); // "cited" is at positions 0-5 (exclusive end)
        assert_eq!(citations[0].cited_text, "cited");
    }

    #[test]
    fn test_citation_indices_with_multiple_citations() {
        // Verify that citation indices are correct when multiple citations close
        let mut tracker = CitationTracker::new();
        tracker.add_token("Before [s:0]first[/s:0] middle [s:1]second[/s:1] after");

        let (clean, citations) = tracker.finalize();
        assert_eq!(clean, "Before first middle second after");
        assert_eq!(citations.len(), 2);

        // First citation: "first" at position 7-12 (exclusive end)
        // "Before " = 7 chars, then "first" = 5 chars
        assert_eq!(citations[0].source_id, 0);
        assert_eq!(citations[0].start_index, 7);
        assert_eq!(citations[0].end_index, 12);
        assert_eq!(citations[0].cited_text, "first");

        // Second citation: "second" at position 20-26 (exclusive end)
        // "Before first middle " = 20 chars, then "second" = 6 chars
        assert_eq!(citations[1].source_id, 1);
        assert_eq!(citations[1].start_index, 20);
        assert_eq!(citations[1].end_index, 26);
        assert_eq!(citations[1].cited_text, "second");
    }
}
