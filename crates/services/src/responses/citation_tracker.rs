//! Citation tracking with state machine for parsing [s:N] tags during LLM streaming
//!
//! This module handles parsing source citations from LLM responses using a state machine
//! that properly handles tags split across tokens. The tracker processes tokens incrementally,
//! removing tags and emitting clean text while tracking citation positions in real-time.
//!
//! ## Citation Emission
//!
//! When a citation closing tag [/s:N] is encountered, the tracker immediately emits a
//! `CompletedCitation` via the `TokenResult` return type. This enables real-time SSE
//! event emission in the streaming pipeline without waiting for message finalization.

/// Result from processing a token through the citation tracker
#[derive(Debug, Clone)]
pub struct TokenResult {
    /// Clean text output (tags removed)
    pub clean_text: String,
    /// Citation that just closed (if any)
    pub completed_citation: Option<CompletedCitation>,
}

/// Citation that has been completed (closing tag encountered)
#[derive(Debug, Clone)]
pub struct CompletedCitation {
    /// Source/reference ID from the citation tag
    pub source_id: usize,
    /// Start index in clean text
    pub start_index: usize,
    /// End index in clean text
    pub end_index: usize,
}

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

    /// Citation that just closed in the current token (for immediate emission)
    just_closed_citation: Option<CompletedCitation>,
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
            token_buffer: String::with_capacity(10),
            clean_position: 0,
            active_citation: None,
            completed_citations: Vec::new(),
            previous_state: None,
            just_closed_citation: None,
        }
    }

    /// Add a token from the LLM stream and return clean output (with tags removed)
    /// Also returns any citation that just completed (closing tag encountered)
    pub fn add_token(&mut self, token: &str) -> TokenResult {
        // Clear any previous token's just-closed citation
        self.just_closed_citation = None;

        let mut clean_text = String::new();
        for ch in token.chars() {
            let output = self.process_char(ch);
            clean_text.push_str(&output);
        }

        TokenResult {
            clean_text,
            completed_citation: self.just_closed_citation.take(),
        }
    }

    /// Extract source ID from buffer at given positions
    /// Parses digits between start_idx and end_idx as usize
    fn parse_digits_from_buffer(&self, start_idx: usize, end_idx: usize) -> Option<usize> {
        if start_idx < end_idx && end_idx <= self.token_buffer.len() {
            self.token_buffer[start_idx..end_idx].parse::<usize>().ok()
        } else {
            None
        }
    }

    /// Flush buffer contents to clean_text and update tracking
    /// Returns the flushed content as a String
    fn do_flush_to_clean_text(&mut self, buffer_content: &str) -> String {
        let mut output = String::new();
        for ch in buffer_content.chars() {
            self.clean_text.push(ch);
            if let Some(ref mut active) = self.active_citation {
                active.accumulated_content.push(ch);
            }
            self.clean_position += 1;
            output.push(ch);
        }
        output
    }

    /// Process a single character through the state machine
    /// Returns String (empty if consumed by tag, otherwise the character(s) to output)
    fn process_char(&mut self, ch: char) -> String {
        match self.current_state {
            TagState::Idle => {
                if ch == '[' {
                    // Start of potential tag
                    self.token_buffer.push(ch);
                    self.previous_state = Some(TagState::Idle);
                    self.current_state = TagState::PartialOpen;
                    String::new() // Don't output yet, wait for next char
                } else {
                    // Regular character - add to clean_text and output
                    self.clean_text.push(ch);
                    if let Some(ref mut active) = self.active_citation {
                        active.accumulated_content.push(ch);
                    }
                    self.clean_position += 1;
                    ch.to_string()
                }
            }

            TagState::PartialOpen => {
                if ch == 's' {
                    self.token_buffer.push(ch);
                    // Might be [s:N]
                    self.current_state = TagState::PartialOpenTag;
                    String::new() // Still buffering
                } else if ch == '/' {
                    self.token_buffer.push(ch);
                    // Might be [/s:N]
                    self.current_state = TagState::PartialCloseTag;
                    String::new() // Still buffering
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
                    String::new() // Still buffering
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
                    String::new()
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
                    if let Some(source_id) =
                        self.parse_digits_from_buffer(3, self.token_buffer.len() - 1)
                    {
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
                        String::new() // Tag consumed, don't output
                    } else {
                        // Failed to parse source_id, flush as literal
                        self.flush_token_buffer_and_restore_state()
                    }
                } else if !ch.is_ascii_digit() {
                    // Invalid (expected ] or another digit), flush as literal
                    self.flush_token_buffer_and_restore_state()
                } else {
                    // Another digit, keep buffering
                    String::new()
                }
            }

            TagState::PartialCloseTag => {
                if ch == 's' {
                    self.token_buffer.push(ch);
                    // [/s found, waiting for :
                    self.current_state = TagState::PartialCloseTagS;
                    String::new()
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
                    String::new()
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
                    String::new()
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
                    if let Some(source_id) =
                        self.parse_digits_from_buffer(4, self.token_buffer.len() - 1)
                    {
                        // Citation is closing - finalize it immediately with correct indices
                        if let Some(active) = self.active_citation.take() {
                            if active.source_id == source_id {
                                tracing::debug!("CitationTracker: Citation tag closed [/s:{}] - indices=[{}, {}], text='{}'", source_id, active.start_index, self.clean_position, active.accumulated_content);
                                let citation = Citation {
                                    start_index: active.start_index,
                                    end_index: self.clean_position,
                                    source_id,
                                    cited_text: active.accumulated_content,
                                };

                                // Store for immediate emission in TokenResult
                                self.just_closed_citation = Some(CompletedCitation {
                                    source_id: citation.source_id,
                                    start_index: citation.start_index,
                                    end_index: citation.end_index,
                                });

                                // Also store in completed_citations for finalization
                                self.completed_citations.push(citation);
                            }
                        }

                        self.current_state = TagState::Idle;
                        self.token_buffer.clear();
                        self.previous_state = None;
                        String::new() // Tag consumed, don't output
                    } else {
                        // Failed to parse source_id, flush as literal
                        self.flush_token_buffer_and_restore_state()
                    }
                } else if !ch.is_ascii_digit() {
                    // Invalid (expected ] or another digit), flush as literal
                    self.flush_token_buffer_and_restore_state()
                } else {
                    // Another digit, keep buffering
                    String::new()
                }
            }

            TagState::InsideTag { source_id } => {
                if ch == '[' {
                    // Might be start of closing tag, begin buffering
                    self.token_buffer.push(ch);
                    self.previous_state = Some(TagState::InsideTag { source_id });
                    self.current_state = TagState::PartialOpen;
                    String::new() // Wait for next char before outputting
                } else {
                    // Regular character inside citation - output and track
                    self.clean_text.push(ch);
                    if let Some(ref mut active) = self.active_citation {
                        active.accumulated_content.push(ch);
                    }
                    self.clean_position += 1;
                    ch.to_string()
                }
            }
        }
    }

    /// Flush token buffer and restore previous state, returning all flushed content
    fn flush_token_buffer_and_restore_state(&mut self) -> String {
        let output = self.do_flush_to_clean_text(&self.token_buffer.clone());
        self.token_buffer.clear();
        self.current_state = self.previous_state.take().unwrap_or(TagState::Idle);
        output // Return ALL characters from the flushed buffer
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
            let buffer_content = self.token_buffer.clone();
            let _ = self.do_flush_to_clean_text(&buffer_content);
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
        assert_eq!(out1.clean_text, "Hello ");
        assert_eq!(out2.clean_text, ""); // Opening tag is consumed
        assert_eq!(out3.clean_text, "world");
        assert_eq!(out4.clean_text, ""); // Closing tag is consumed
        assert!(out4.completed_citation.is_some()); // Citation closed in last token

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
        assert_eq!(out1.clean_text, "Hello ");
        assert_eq!(out2.clean_text, ""); // Buffering partial tag
        assert_eq!(out3.clean_text, ""); // Closing partial tag
        assert_eq!(out4.clean_text, "world");
        assert_eq!(out5.clean_text, ""); // Closing tag consumed
        assert!(out5.completed_citation.is_some()); // Citation closed in last token

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

        let out1 = tracker.add_token("[s:0]");
        assert_eq!(out1.clean_text, "");
        assert!(out1.completed_citation.is_none());

        // Token 2: Opening citation tag [s:0]
        let out2 = tracker.add_token("[s:0]");
        assert_eq!(out2.clean_text, "");
        assert!(out2.completed_citation.is_none());

        // Token 3: Citation content "world"
        let out3 = tracker.add_token("world");
        assert_eq!(out3.clean_text, "world");
        assert!(out3.completed_citation.is_none());

        // Token 4: Closing citation tag [/s:0]
        // THIS IS THE KEY: When this token is processed, the citation closes
        // and should be emitted in the TokenResult
        let out4 = tracker.add_token("[/s:0]");
        assert_eq!(out4.clean_text, "");
        assert!(out4.completed_citation.is_some());

        // Verify the completed citation has correct indices
        let citation = out4.completed_citation.unwrap();
        assert_eq!(citation.source_id, 0);
        assert_eq!(citation.start_index, 0); // "Hello " = 6 chars
        assert_eq!(citation.end_index, 5); // "Hello world" = 11 chars

        // Token 5: Remaining text
        let out5 = tracker.add_token(" end");
        assert_eq!(out5.clean_text, " end");
        assert!(out5.completed_citation.is_none());

        // Finalize still works correctly
        let (clean, citations) = tracker.finalize();
        assert_eq!(clean, "world end");
        assert_eq!(citations.len(), 1);
        assert_eq!(citations[0].source_id, 0);
        assert_eq!(citations[0].start_index, 0);
        assert_eq!(citations[0].end_index, 5);
    }

    #[test]
    fn test_multiple_citations_real_time_emission() {
        // Verify that each citation is emitted exactly once when it closes
        let mut tracker = CitationTracker::new();

        // First citation
        let r1 = tracker.add_token("Start [s:0]first[/s:0]");
        // "Start " = 6 chars, then "first" = 5 chars
        assert!(r1.completed_citation.is_some());
        let c1 = r1.completed_citation.unwrap();
        assert_eq!(c1.source_id, 0);
        assert_eq!(c1.start_index, 6);
        assert_eq!(c1.end_index, 11);

        // Gap
        let r2 = tracker.add_token(" middle ");
        assert_eq!(r2.clean_text, " middle ");
        assert!(r2.completed_citation.is_none());

        // Second citation
        let r3 = tracker.add_token("[s:1]second[/s:1]");
        // "Start first middle " = 19 chars, then "second" = 6 chars
        assert!(r3.completed_citation.is_some());
        let c2 = r3.completed_citation.unwrap();
        assert_eq!(c2.source_id, 1);
        assert_eq!(c2.start_index, 19);
        assert_eq!(c2.end_index, 25);

        // Verify finalization
        let (clean, citations) = tracker.finalize();
        assert_eq!(clean, "Start first middle second");
        assert_eq!(citations.len(), 2);
        assert_eq!(citations[0].source_id, 0);
        assert_eq!(citations[1].source_id, 1);
    }

    #[test]
    fn test_split_closing_tag_emits_citation() {
        // Verify that even when the closing tag is split across tokens,
        // the citation is still emitted as soon as it closes
        let mut tracker = CitationTracker::new();

        tracker.add_token("Hello [s:0]world");
        // Citation open but not closed yet
        let r1 = tracker.add_token("[/s");
        assert!(r1.completed_citation.is_none()); // Still buffering closing tag

        let r2 = tracker.add_token(":0]");
        // NOW the closing tag is complete
        assert!(r2.completed_citation.is_some());
        let citation = r2.completed_citation.unwrap();
        assert_eq!(citation.source_id, 0);
        assert_eq!(citation.start_index, 6);
        assert_eq!(citation.end_index, 11);

        let (clean, citations) = tracker.finalize();
        assert_eq!(clean, "Hello world");
        assert_eq!(citations.len(), 1);
    }
}
