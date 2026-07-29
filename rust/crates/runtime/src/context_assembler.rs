//! # Context Assembler
//!
//! Unified system prompt injection with fixed priority stack for the C (Context Management) layer.
//!
//! ## Priority Ordering (fixed, runtime-immutable)
//!
//! Lower numeric value = higher priority. System prompts always come first,
//! user input always comes last. This ordering is deterministic and cannot be
//! modified at runtime, ensuring cache protection.
//!
//! | Source      | Priority | Description                          |
//! |------------|----------|--------------------------------------|
//! | System     | 0        | Core system prompt (highest)         |
//! | Tools      | 1        | Tool definitions and schemas         |
//! | Memory     | 2        | Semantic memory / long-term context  |
//! | Goal       | 3        | Current goal / objective             |
//! | GitContext  | 4        | Git context (branch, diff, staged)   |
//! | History    | 5        | Conversation history                 |
//! | User       | 6        | Latest user input (lowest)           |
//!
//! ## Token Budget Management
//!
//! Each source has a configurable maximum token budget. When the total
//! assembled prompt exceeds the global budget, lowest-priority blocks are
//! truncated first (from User upward).
//!
//! ## Cache Protection
//!
//! The fixed priority stack ensures that the same set of context sources
//! always produces the same assembled prompt structure, enabling reliable
//! caching and deterministic behavior.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// ContextSource — priority enum
// ---------------------------------------------------------------------------

/// Represents a context source with a fixed priority.
///
/// Lower numeric value = higher importance.
/// System prompts (0) are always included first; User input (6) is truncated
/// first when budget is exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextSource {
    /// Core system prompt — always included, highest priority.
    System = 0,
    /// Tool definitions and schemas — high priority, defines available tools.
    Tools = 1,
    /// Semantic memory / long-term context — persistent knowledge.
    Memory = 2,
    /// Current goal / objective — what the agent is working toward.
    Goal = 3,
    /// Git context (branch, diff, staged files) — repository awareness.
    GitContext = 4,
    /// Conversation history — prior turns in the dialogue.
    History = 5,
    /// Latest user input — lowest priority, truncated first.
    User = 6,
}

impl ContextSource {
    /// Returns all variants in priority order (ascending).
    pub fn all_sorted() -> Vec<ContextSource> {
        vec![
            ContextSource::System,
            ContextSource::Tools,
            ContextSource::Memory,
            ContextSource::Goal,
            ContextSource::GitContext,
            ContextSource::History,
            ContextSource::User,
        ]
    }

    /// Returns the numeric priority (lower = higher importance).
    pub fn priority(&self) -> u8 {
        *self as u8
    }

    /// Returns a human-readable label for the source.
    pub fn label(&self) -> &'static str {
        match self {
            ContextSource::System => "system",
            ContextSource::Tools => "tools",
            ContextSource::Memory => "memory",
            ContextSource::Goal => "goal",
            ContextSource::GitContext => "git_context",
            ContextSource::History => "history",
            ContextSource::User => "user",
        }
    }

    /// Parse from string label (case-insensitive).
    pub fn from_label(label: &str) -> Option<ContextSource> {
        match label.to_lowercase().as_str() {
            "system" => Some(ContextSource::System),
            "tools" => Some(ContextSource::Tools),
            "memory" => Some(ContextSource::Memory),
            "goal" => Some(ContextSource::Goal),
            "git_context" | "gitcontext" => Some(ContextSource::GitContext),
            "history" => Some(ContextSource::History),
            "user" => Some(ContextSource::User),
            _ => None,
        }
    }
}

impl std::fmt::Display for ContextSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.label())
    }
}

// ---------------------------------------------------------------------------
// ContextBlock — single piece of context
// ---------------------------------------------------------------------------

/// A single block of context content, tagged with its source and token estimate.
///
/// Blocks are the atomic unit of context assembly. Each block belongs to exactly
/// one [`ContextSource`] and carries an estimated token count for budget management.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextBlock {
    /// The source this block belongs to (determines priority).
    pub source: ContextSource,
    /// The actual text content of the block.
    pub content: String,
    /// Estimated token count for the content.
    pub token_estimate: usize,
}

impl ContextBlock {
    /// Create a new context block.
    pub fn new(source: ContextSource, content: String, token_estimate: usize) -> Self {
        Self {
            source,
            content,
            token_estimate,
        }
    }

    /// Create a block with an automatically estimated token count.
    ///
    /// Uses a simple heuristic: ~4 characters per token (rough approximation
    /// for English text with typical subword tokenizers).
    pub fn new_auto_estimate(source: ContextSource, content: String) -> Self {
        let token_estimate = (content.len() as f64 / 4.0).ceil() as usize;
        Self {
            source,
            content,
            token_estimate: token_estimate.max(1),
        }
    }

    /// Truncate content to fit within a token budget.
    ///
    /// If the content exceeds `max_tokens`, it is truncated and "..." is appended.
    /// Returns a new block with the truncated content and updated token estimate.
    pub fn truncate_to(&self, max_tokens: usize) -> ContextBlock {
        if self.token_estimate <= max_tokens {
            return self.clone();
        }

        // Approximate character count for the token budget (4 chars per token)
        let max_chars = max_tokens.saturating_sub(1) * 4; // -1 for "..."
        let truncated: String = self.content.chars().take(max_chars).collect();
        let new_content = format!("{}...", truncated);

        ContextBlock {
            source: self.source,
            content: new_content,
            token_estimate: max_tokens,
        }
    }
}

// ---------------------------------------------------------------------------
// AssembledPrompt — the output of assembly
// ---------------------------------------------------------------------------

/// The result of assembling context blocks into a single prompt.
///
/// Contains all blocks in priority order, with total token tracking and
/// cache break point information for invalidation strategies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssembledPrompt {
    /// Blocks in priority order (system first, user last).
    pub blocks: Vec<ContextBlock>,
    /// Total estimated tokens across all blocks.
    pub total_token_estimate: usize,
    /// Byte offset where volatile (frequently-changing) content begins.
    ///
    /// Content before this point is "stable" and suitable for caching.
    /// Content at and after this point changes frequently (e.g., user input,
    /// conversation history) and should not be cached.
    pub cache_break_point: usize,
}

impl AssembledPrompt {
    /// Create an empty assembled prompt.
    pub fn empty() -> Self {
        Self {
            blocks: Vec::new(),
            total_token_estimate: 0,
            cache_break_point: 0,
        }
    }

    /// Render the assembled prompt as a single string.
    ///
    /// Blocks are joined with double newlines. Each block is prefixed with
    /// a section header indicating its source.
    pub fn render(&self) -> String {
        if self.blocks.is_empty() {
            return String::new();
        }

        let mut parts: Vec<String> = Vec::with_capacity(self.blocks.len());
        for block in &self.blocks {
            parts.push(format!("# [{}]\n{}", block.source.label(), block.content));
        }
        parts.join("\n\n")
    }

    /// Render as a flat string without section headers.
    pub fn render_flat(&self) -> String {
        self.blocks
            .iter()
            .map(|b| b.content.as_str())
            .collect::<Vec<&str>>()
            .join("\n\n")
    }

    /// Count the number of blocks from a specific source.
    pub fn count_blocks_for_source(&self, source: ContextSource) -> usize {
        self.blocks.iter().filter(|b| b.source == source).count()
    }

    /// Get total tokens for a specific source.
    pub fn tokens_for_source(&self, source: ContextSource) -> usize {
        self.blocks
            .iter()
            .filter(|b| b.source == source)
            .map(|b| b.token_estimate)
            .sum()
    }

    /// Check if any blocks from the given source are present.
    pub fn has_source(&self, source: ContextSource) -> bool {
        self.blocks.iter().any(|b| b.source == source)
    }

    /// Returns the stable portion (before cache_break_point) as a string.
    pub fn stable_content(&self) -> String {
        if self.cache_break_point == 0 {
            return self.render();
        }
        let full = self.render();
        full[..self.cache_break_point.min(full.len())].to_string()
    }

    /// Returns the volatile portion (from cache_break_point onward) as a string.
    pub fn volatile_content(&self) -> String {
        if self.cache_break_point == 0 {
            return String::new();
        }
        let full = self.render();
        full[self.cache_break_point.min(full.len())..].to_string()
    }
}

// ---------------------------------------------------------------------------
// TokenBudget — per-source budget configuration
// ---------------------------------------------------------------------------

/// Token budget configuration for the assembly process.
///
/// Controls both the global budget and per-source limits. When the total
/// assembled prompt exceeds the global budget, lowest-priority sources are
/// truncated first.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenBudget {
    /// Maximum total tokens for the assembled prompt.
    pub max_total: usize,
    /// Per-source maximum token limits.
    ///
    /// If a source is not present in this map, it has no per-source limit
    /// (only constrained by `max_total`).
    pub per_source: HashMap<ContextSource, usize>,
}

impl TokenBudget {
    /// Create a new budget with a global maximum and no per-source limits.
    pub fn new(max_total: usize) -> Self {
        Self {
            max_total,
            per_source: HashMap::new(),
        }
    }

    /// Create a budget with both global and per-source limits.
    pub fn with_per_source(mut self, source: ContextSource, max_tokens: usize) -> Self {
        self.per_source.insert(source, max_tokens);
        self
    }

    /// Get the per-source limit for a given source, if set.
    pub fn limit_for(&self, source: ContextSource) -> Option<usize> {
        self.per_source.get(&source).copied()
    }

    /// Default budget configuration for typical LLM interactions.
    ///
    /// - Global: 120,000 tokens
    /// - System: 8,000
    /// - Tools: 16,000
    /// - Memory: 6,000
    /// - Goal: 4,000
    /// - GitContext: 12,000
    /// - History: 50,000
    /// - User: 24,000
    pub fn default_claude() -> Self {
        Self {
            max_total: 120_000,
            per_source: HashMap::from([
                (ContextSource::System, 8_000),
                (ContextSource::Tools, 16_000),
                (ContextSource::Memory, 6_000),
                (ContextSource::Goal, 4_000),
                (ContextSource::GitContext, 12_000),
                (ContextSource::History, 50_000),
                (ContextSource::User, 24_000),
            ]),
        }
    }

    /// Compact budget for smaller context windows.
    pub fn compact() -> Self {
        Self {
            max_total: 32_000,
            per_source: HashMap::from([
                (ContextSource::System, 2_000),
                (ContextSource::Tools, 4_000),
                (ContextSource::Memory, 2_000),
                (ContextSource::Goal, 1_000),
                (ContextSource::GitContext, 4_000),
                (ContextSource::History, 12_000),
                (ContextSource::User, 7_000),
            ]),
        }
    }

    /// 根据模型 context window 缩放预算。
    ///
    /// - `<= 200K`: 返回 `default_claude()`(120K 全局)
    /// - `> 200K`:  按比例缩放,上限 `default_claude()` × 4
    pub fn for_context_window(context_window: u32) -> Self {
        if context_window <= 200_000 {
            return Self::default_claude();
        }
        // 按比例缩放,最小保持 default_claude
        let scale = ((context_window as f64) / 200_000.0).min(4.0);
        let max_total = (120_000.0 * scale) as usize;
        let scale_factor = |base: usize| -> usize { ((base as f64) * scale) as usize };
        Self {
            max_total,
            per_source: HashMap::from([
                (ContextSource::System, scale_factor(8_000)),
                (ContextSource::Tools, scale_factor(16_000)),
                (ContextSource::Memory, scale_factor(6_000)),
                (ContextSource::Goal, scale_factor(4_000)),
                (ContextSource::GitContext, scale_factor(12_000)),
                (ContextSource::History, scale_factor(50_000)),
                (ContextSource::User, scale_factor(24_000)),
            ]),
        }
    }
}

impl Default for TokenBudget {
    fn default() -> Self {
        Self::default_claude()
    }
}

// ---------------------------------------------------------------------------
// CacheStrategy — controls cache break point behavior
// ---------------------------------------------------------------------------

/// Strategy for determining the cache break point in assembled prompts.
///
/// The cache break point divides the prompt into "stable" and "volatile"
/// regions. Stable content (before the break point) rarely changes and is
/// suitable for caching. Volatile content (after the break point) changes
/// frequently and should not be cached.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheStrategy {
    /// Cache break point is after all stable sources (System, Tools, Memory, Goal, GitContext).
    /// History and User are considered volatile.
    #[default]
    StablePrefix,
    /// Cache break point is after System only. Everything else is volatile.
    /// Most conservative — smallest cacheable region.
    SystemOnly,
    /// No cache break point — entire prompt is treated as volatile.
    /// Useful for debugging or when caching is not desired.
    None,
}

// ---------------------------------------------------------------------------
// ContextAssembler — the main assembly engine
// ---------------------------------------------------------------------------

/// Assembles context blocks into a single prioritized prompt.
///
/// # Cache Protection
///
/// The priority ordering is **fixed and runtime-immutable**. This ensures:
/// - Deterministic assembly: same inputs always produce same output structure
/// - Reliable caching: cache keys can be computed from stable prefixes
/// - Predictable truncation: budget overflows always cut from lowest priority
///
/// # Example
///
/// ```rust,ignore
/// use crate::context_assembler::{ContextAssembler, ContextSource, ContextBlock, TokenBudget};
///
/// let mut assembler = ContextAssembler::new(TokenBudget::default_claude());
///
/// assembler.add_block(ContextBlock::new(
///     ContextSource::System,
///     "You are a helpful assistant.".to_string(),
///     8,
/// ));
///
/// assembler.add_block(ContextBlock::new(
///     ContextSource::User,
///     "What is Rust?".to_string(),
///     4,
/// ));
///
/// let prompt = assembler.assemble();
/// println!("{}", prompt.render());
/// ```
#[derive(Debug, Clone)]
pub struct ContextAssembler {
    /// All collected context blocks, grouped by source.
    blocks_by_source: HashMap<ContextSource, Vec<ContextBlock>>,
    /// Token budget configuration.
    budget: TokenBudget,
    /// Cache strategy for determining the break point.
    cache_strategy: CacheStrategy,
}

impl ContextAssembler {
    /// Create a new assembler with the given token budget.
    pub fn new(budget: TokenBudget) -> Self {
        Self {
            blocks_by_source: HashMap::new(),
            budget,
            cache_strategy: CacheStrategy::default(),
        }
    }

    /// Create a new assembler with default budget and strategy.
    pub fn default_with_strategy(cache_strategy: CacheStrategy) -> Self {
        Self {
            blocks_by_source: HashMap::new(),
            budget: TokenBudget::default(),
            cache_strategy,
        }
    }

    /// Set the cache strategy.
    pub fn with_cache_strategy(mut self, strategy: CacheStrategy) -> Self {
        self.cache_strategy = strategy;
        self
    }

    /// Add a single context block.
    pub fn add_block(&mut self, block: ContextBlock) {
        self.blocks_by_source
            .entry(block.source)
            .or_default()
            .push(block);
    }

    /// Add multiple context blocks.
    pub fn add_blocks(&mut self, blocks: impl IntoIterator<Item = ContextBlock>) {
        for block in blocks {
            self.add_block(block);
        }
    }

    /// Add a context block from raw parameters (convenience method).
    pub fn add(&mut self, source: ContextSource, content: String, token_estimate: usize) {
        self.add_block(ContextBlock::new(source, content, token_estimate));
    }

    /// Add a context block with auto-estimated token count.
    pub fn add_auto(&mut self, source: ContextSource, content: String) {
        self.add_block(ContextBlock::new_auto_estimate(source, content));
    }

    /// Clear all blocks for a specific source.
    pub fn clear_source(&mut self, source: ContextSource) {
        self.blocks_by_source.remove(&source);
    }

    /// Clear all blocks.
    pub fn clear(&mut self) {
        self.blocks_by_source.clear();
    }

    /// Get the number of blocks for a specific source.
    pub fn block_count(&self, source: ContextSource) -> usize {
        self.blocks_by_source.get(&source).map_or(0, |v| v.len())
    }

    /// Get total blocks across all sources.
    pub fn total_block_count(&self) -> usize {
        self.blocks_by_source.values().map(|v| v.len()).sum()
    }

    /// Assemble all context blocks into a single prompt.
    ///
    /// # Algorithm
    ///
    /// 1. **Enforce per-source limits**: Truncate each source's blocks to fit
    ///    within its per-source token budget.
    /// 2. **Sort by priority**: Arrange blocks in ascending priority order
    ///    (System first, User last).
    /// 3. **Enforce global budget**: If total tokens exceed `max_total`,
    ///    truncate blocks from lowest priority (User) upward until the budget
    ///    is satisfied.
    /// 4. **Compute cache break point**: Determine the byte offset where
    ///    volatile content begins, based on the configured cache strategy.
    pub fn assemble(&self) -> AssembledPrompt {
        // Step 1: Enforce per-source limits
        let mut processed_blocks: Vec<ContextBlock> = Vec::new();

        for source in ContextSource::all_sorted() {
            if let Some(blocks) = self.blocks_by_source.get(&source) {
                let per_source_limit = self.budget.limit_for(source);

                let mut source_blocks: Vec<ContextBlock> = blocks.to_vec();

                // Apply per-source truncation if needed
                if let Some(limit) = per_source_limit {
                    let total_tokens: usize = source_blocks.iter().map(|b| b.token_estimate).sum();

                    if total_tokens > limit {
                        // Truncate individual blocks from the end (last added = least important within source)
                        let mut remaining = limit;
                        let mut truncated = Vec::new();

                        for block in source_blocks.iter() {
                            if remaining == 0 {
                                break;
                            }
                            if block.token_estimate <= remaining {
                                truncated.push(block.clone());
                                remaining -= block.token_estimate;
                            } else {
                                // Partially include this block
                                truncated.push(block.truncate_to(remaining));
                                remaining = 0;
                            }
                        }
                        source_blocks = truncated;
                    }
                }

                processed_blocks.extend(source_blocks);
            }
        }

        // Step 2: Blocks are already in priority order from the iteration above

        // Step 3: Enforce global budget
        let total_tokens: usize = processed_blocks.iter().map(|b| b.token_estimate).sum();

        if total_tokens > self.budget.max_total {
            processed_blocks = self.truncate_to_global_budget(processed_blocks);
        }

        // Step 4: Compute cache break point and total tokens
        let final_total: usize = processed_blocks.iter().map(|b| b.token_estimate).sum();
        let cache_break_point = self.compute_cache_break_point(&processed_blocks);

        AssembledPrompt {
            blocks: processed_blocks,
            total_token_estimate: final_total,
            cache_break_point,
        }
    }

    /// Truncate blocks from lowest priority upward to fit within global budget.
    ///
    /// This removes entire blocks starting from the end (User source) and
    /// working upward. If a block cannot be fully removed without going
    /// under budget, it is partially truncated.
    fn truncate_to_global_budget(&self, mut blocks: Vec<ContextBlock>) -> Vec<ContextBlock> {
        let budget = self.budget.max_total;

        // Remove blocks from the end (lowest priority) until we're within budget
        while !blocks.is_empty() {
            let current_total: usize = blocks.iter().map(|b| b.token_estimate).sum();
            if current_total <= budget {
                break;
            }

            let last = blocks.last_mut().expect("blocks non-empty per while condition");

            // Calculate how much we need to save
            let excess = current_total - budget;

            if last.token_estimate <= excess {
                // Remove this entire block
                blocks.pop();
            } else {
                // Partially truncate this block
                let new_tokens = last.token_estimate - excess;
                *last = last.truncate_to(new_tokens);
                break;
            }
        }

        blocks
    }

    /// Compute the cache break point based on the configured strategy.
    ///
    /// Returns the byte offset in the rendered prompt where volatile content
    /// begins. Content before this offset is considered "stable" (cacheable).
    fn compute_cache_break_point(&self, blocks: &[ContextBlock]) -> usize {
        match self.cache_strategy {
            CacheStrategy::None => 0,
            CacheStrategy::SystemOnly => {
                // Break point is after all System blocks
                let mut offset = 0;
                for block in blocks {
                    let header = format!("# [{}]\n", block.source.label());
                    let block_text = format!("{}{}", header, block.content);

                    if block.source == ContextSource::System {
                        offset += block_text.len() + 2; // +2 for "\n\n"
                    } else {
                        break;
                    }
                }
                offset
            }
            CacheStrategy::StablePrefix => {
                // Break point is after all stable sources (System through GitContext)
                let stable_sources: Vec<ContextSource> = vec![
                    ContextSource::System,
                    ContextSource::Tools,
                    ContextSource::Memory,
                    ContextSource::Goal,
                    ContextSource::GitContext,
                ];

                let mut offset = 0;
                let mut past_stable = false;

                for block in blocks {
                    let header = format!("# [{}]\n", block.source.label());
                    let block_text = format!("{}{}", header, block.content);

                    if stable_sources.contains(&block.source) {
                        if past_stable {
                            // We've already seen volatile content, shouldn't happen
                            // with proper priority ordering, but handle gracefully
                        }
                        offset += block_text.len() + 2; // +2 for "\n\n"
                    } else {
                        past_stable = true;
                        // Don't add to offset — this is volatile content
                    }
                }

                offset
            }
        }
    }
}

impl Default for ContextAssembler {
    fn default() -> Self {
        Self::new(TokenBudget::default())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_source_priority_ordering() {
        let sources = ContextSource::all_sorted();
        assert_eq!(sources.len(), 7);
        assert_eq!(sources[0], ContextSource::System);
        assert_eq!(sources[6], ContextSource::User);

        // Verify monotonic priority
        for i in 1..sources.len() {
            assert!(sources[i].priority() > sources[i - 1].priority());
        }
    }

    #[test]
    fn test_context_source_label_roundtrip() {
        for source in ContextSource::all_sorted() {
            let label = source.label();
            let parsed = ContextSource::from_label(label);
            assert_eq!(parsed, Some(source));
        }
    }

    #[test]
    fn test_context_block_truncation() {
        let block = ContextBlock::new(
            ContextSource::User,
            "This is a long piece of text that should be truncated".to_string(),
            100,
        );

        let truncated = block.truncate_to(50);
        assert_eq!(truncated.token_estimate, 50);
        assert!(truncated.content.ends_with("..."));
    }

    #[test]
    fn test_context_block_no_truncation_needed() {
        let block = ContextBlock::new(ContextSource::System, "Short text".to_string(), 5);

        let truncated = block.truncate_to(10);
        assert_eq!(truncated.token_estimate, 5);
        assert_eq!(truncated.content, "Short text");
    }

    #[test]
    fn test_auto_estimate() {
        let block = ContextBlock::new_auto_estimate(
            ContextSource::User,
            "Hello world this is a test".to_string(),
        );
        // 27 chars / 4 ≈ 7 tokens
        assert!(block.token_estimate >= 7);
        assert!(block.token_estimate <= 8);
    }

    #[test]
    fn test_basic_assembly() {
        let mut assembler = ContextAssembler::new(TokenBudget::new(1000));

        assembler.add(ContextSource::System, "You are helpful.".to_string(), 5);
        assembler.add(ContextSource::User, "Hello!".to_string(), 2);

        let prompt = assembler.assemble();

        assert_eq!(prompt.blocks.len(), 2);
        assert_eq!(prompt.blocks[0].source, ContextSource::System);
        assert_eq!(prompt.blocks[1].source, ContextSource::User);
        assert_eq!(prompt.total_token_estimate, 7);
    }

    #[test]
    fn test_priority_ordering_in_assembly() {
        let mut assembler = ContextAssembler::new(TokenBudget::new(10000));

        // Add in reverse priority order
        assembler.add(ContextSource::User, "User input".to_string(), 10);
        assembler.add(ContextSource::History, "History".to_string(), 10);
        assembler.add(ContextSource::GitContext, "Git".to_string(), 10);
        assembler.add(ContextSource::Goal, "Goal".to_string(), 10);
        assembler.add(ContextSource::Memory, "Memory".to_string(), 10);
        assembler.add(ContextSource::Tools, "Tools".to_string(), 10);
        assembler.add(ContextSource::System, "System".to_string(), 10);

        let prompt = assembler.assemble();

        // Should be in priority order regardless of insertion order
        assert_eq!(prompt.blocks[0].source, ContextSource::System);
        assert_eq!(prompt.blocks[1].source, ContextSource::Tools);
        assert_eq!(prompt.blocks[2].source, ContextSource::Memory);
        assert_eq!(prompt.blocks[3].source, ContextSource::Goal);
        assert_eq!(prompt.blocks[4].source, ContextSource::GitContext);
        assert_eq!(prompt.blocks[5].source, ContextSource::History);
        assert_eq!(prompt.blocks[6].source, ContextSource::User);
    }

    #[test]
    fn test_per_source_truncation() {
        let budget = TokenBudget::new(10000).with_per_source(ContextSource::User, 20);

        let mut assembler = ContextAssembler::new(budget);

        assembler.add(ContextSource::System, "System".to_string(), 10);
        assembler.add(ContextSource::User, "A".to_string(), 15);
        assembler.add(ContextSource::User, "B".to_string(), 15);

        let prompt = assembler.assemble();

        // User blocks total 30, but per-source limit is 20
        // Should include first user block (15) and truncate second (to 5)
        let user_tokens: usize = prompt
            .blocks
            .iter()
            .filter(|b| b.source == ContextSource::User)
            .map(|b| b.token_estimate)
            .sum();
        assert_eq!(user_tokens, 20);
    }

    #[test]
    fn test_global_budget_truncation() {
        let budget = TokenBudget::new(25);
        let mut assembler = ContextAssembler::new(budget);

        assembler.add(ContextSource::System, "System prompt".to_string(), 10);
        assembler.add(ContextSource::Tools, "Tool definitions".to_string(), 10);
        assembler.add(ContextSource::User, "User question".to_string(), 10);

        let prompt = assembler.assemble();

        // Total is 30, budget is 25
        // User block should be truncated to 5
        assert!(prompt.total_token_estimate <= 25);
        assert_eq!(prompt.blocks[0].source, ContextSource::System);
        assert_eq!(prompt.blocks[1].source, ContextSource::Tools);
        assert!(prompt
            .blocks
            .iter()
            .any(|b| b.source == ContextSource::User));
    }

    #[test]
    fn test_global_budget_block_removal() {
        let budget = TokenBudget::new(20);
        let mut assembler = ContextAssembler::new(budget);

        assembler.add(ContextSource::System, "System".to_string(), 10);
        assembler.add(ContextSource::Tools, "Tools".to_string(), 10);
        assembler.add(ContextSource::User, "User".to_string(), 10);

        let prompt = assembler.assemble();

        // Total is 30, budget is 20
        // User block should be entirely removed
        assert_eq!(prompt.total_token_estimate, 20);
        assert!(!prompt.has_source(ContextSource::User));
    }

    #[test]
    fn test_cache_break_point_stable_prefix() {
        let mut assembler = ContextAssembler::new(TokenBudget::new(10000))
            .with_cache_strategy(CacheStrategy::StablePrefix);

        assembler.add(ContextSource::System, "System".to_string(), 10);
        assembler.add(ContextSource::Tools, "Tools".to_string(), 10);
        assembler.add(ContextSource::History, "History".to_string(), 10);
        assembler.add(ContextSource::User, "User".to_string(), 10);

        let prompt = assembler.assemble();

        // Cache break point should be after System + Tools (stable sources)
        // and before History + User (volatile sources)
        assert!(prompt.cache_break_point > 0);

        let stable = prompt.stable_content();
        let volatile = prompt.volatile_content();

        assert!(stable.contains("system"));
        assert!(stable.contains("tools"));
        assert!(!stable.contains("history"));
        assert!(!stable.contains("user"));

        assert!(volatile.contains("history"));
        assert!(volatile.contains("user"));
    }

    #[test]
    fn test_cache_break_point_system_only() {
        let mut assembler = ContextAssembler::new(TokenBudget::new(10000))
            .with_cache_strategy(CacheStrategy::SystemOnly);

        assembler.add(ContextSource::System, "System".to_string(), 10);
        assembler.add(ContextSource::Tools, "Tools".to_string(), 10);
        assembler.add(ContextSource::User, "User".to_string(), 10);

        let prompt = assembler.assemble();

        let stable = prompt.stable_content();
        assert!(stable.contains("system"));
        assert!(!stable.contains("tools"));
    }

    #[test]
    fn test_cache_break_point_none() {
        let mut assembler =
            ContextAssembler::new(TokenBudget::new(10000)).with_cache_strategy(CacheStrategy::None);

        assembler.add(ContextSource::System, "System".to_string(), 10);
        assembler.add(ContextSource::User, "User".to_string(), 10);

        let prompt = assembler.assemble();
        assert_eq!(prompt.cache_break_point, 0);
    }

    #[test]
    fn test_render() {
        let mut assembler = ContextAssembler::new(TokenBudget::new(1000));

        assembler.add(ContextSource::System, "You are helpful.".to_string(), 5);
        assembler.add(ContextSource::User, "Hello!".to_string(), 2);

        let prompt = assembler.assemble();
        let rendered = prompt.render();

        assert!(rendered.contains("# [system]"));
        assert!(rendered.contains("You are helpful."));
        assert!(rendered.contains("# [user]"));
        assert!(rendered.contains("Hello!"));
    }

    #[test]
    fn test_render_flat() {
        let mut assembler = ContextAssembler::new(TokenBudget::new(1000));

        assembler.add(ContextSource::System, "System text.".to_string(), 5);
        assembler.add(ContextSource::User, "User text.".to_string(), 2);

        let prompt = assembler.assemble();
        let flat = prompt.render_flat();

        assert!(!flat.contains("# ["));
        assert!(flat.contains("System text."));
        assert!(flat.contains("User text."));
    }

    #[test]
    fn test_empty_assembly() {
        let assembler = ContextAssembler::new(TokenBudget::new(1000));
        let prompt = assembler.assemble();

        assert!(prompt.blocks.is_empty());
        assert_eq!(prompt.total_token_estimate, 0);
        assert_eq!(prompt.cache_break_point, 0);
        assert!(prompt.render().is_empty());
    }

    #[test]
    fn test_clear_source() {
        let mut assembler = ContextAssembler::new(TokenBudget::new(1000));

        assembler.add(ContextSource::System, "System".to_string(), 5);
        assembler.add(ContextSource::User, "User".to_string(), 2);

        assembler.clear_source(ContextSource::User);

        let prompt = assembler.assemble();
        assert_eq!(prompt.blocks.len(), 1);
        assert_eq!(prompt.blocks[0].source, ContextSource::System);
    }

    #[test]
    fn test_clear_all() {
        let mut assembler = ContextAssembler::new(TokenBudget::new(1000));

        assembler.add(ContextSource::System, "System".to_string(), 5);
        assembler.add(ContextSource::User, "User".to_string(), 2);

        assembler.clear();

        let prompt = assembler.assemble();
        assert!(prompt.blocks.is_empty());
    }

    #[test]
    fn test_multiple_blocks_per_source() {
        let mut assembler = ContextAssembler::new(TokenBudget::new(1000));

        assembler.add(ContextSource::System, "System part 1".to_string(), 5);
        assembler.add(ContextSource::System, "System part 2".to_string(), 5);
        assembler.add(ContextSource::User, "User question".to_string(), 5);

        let prompt = assembler.assemble();
        assert_eq!(prompt.count_blocks_for_source(ContextSource::System), 2);
        assert_eq!(prompt.count_blocks_for_source(ContextSource::User), 1);
        assert_eq!(prompt.total_token_estimate, 15);
    }

    #[test]
    fn test_tokens_for_source() {
        let mut assembler = ContextAssembler::new(TokenBudget::new(1000));

        assembler.add(ContextSource::System, "A".to_string(), 10);
        assembler.add(ContextSource::System, "B".to_string(), 15);
        assembler.add(ContextSource::User, "C".to_string(), 5);

        let prompt = assembler.assemble();
        assert_eq!(prompt.tokens_for_source(ContextSource::System), 25);
        assert_eq!(prompt.tokens_for_source(ContextSource::User), 5);
    }

    #[test]
    fn test_compact_budget() {
        let budget = TokenBudget::compact();
        assert_eq!(budget.max_total, 32_000);
        assert_eq!(budget.limit_for(ContextSource::System), Some(2_000));
        assert_eq!(budget.limit_for(ContextSource::User), Some(7_000));
    }

    #[test]
    fn test_default_budget() {
        let budget = TokenBudget::default_claude();
        assert_eq!(budget.max_total, 120_000);
        assert_eq!(budget.limit_for(ContextSource::System), Some(8_000));
        assert_eq!(budget.limit_for(ContextSource::History), Some(50_000));
    }

    #[test]
    fn test_deterministic_assembly() {
        // Same inputs should always produce same output
        let budget = TokenBudget::new(1000);

        let mut assembler1 = ContextAssembler::new(budget.clone());
        assembler1.add(ContextSource::User, "Hello".to_string(), 5);
        assembler1.add(ContextSource::System, "System".to_string(), 5);

        let mut assembler2 = ContextAssembler::new(budget);
        assembler2.add(ContextSource::User, "Hello".to_string(), 5);
        assembler2.add(ContextSource::System, "System".to_string(), 5);

        let prompt1 = assembler1.assemble();
        let prompt2 = assembler2.assemble();

        assert_eq!(prompt1.render(), prompt2.render());
        assert_eq!(prompt1.total_token_estimate, prompt2.total_token_estimate);
        assert_eq!(prompt1.cache_break_point, prompt2.cache_break_point);
    }

    #[test]
    fn test_serde_roundtrip() {
        let source = ContextSource::Memory;
        let json = serde_json::to_string(&source).unwrap();
        let deserialized: ContextSource = serde_json::from_str(&json).unwrap();
        assert_eq!(source, deserialized);
    }

    #[test]
    fn test_token_budget_serde() {
        let budget = TokenBudget::default_claude();
        let json = serde_json::to_string_pretty(&budget).unwrap();
        let deserialized: TokenBudget = serde_json::from_str(&json).unwrap();
        assert_eq!(budget.max_total, deserialized.max_total);
    }

    #[test]
    fn test_budget_for_context_window_default_for_200k() {
        let budget = TokenBudget::for_context_window(200_000);
        assert_eq!(budget.max_total, 120_000);
    }

    #[test]
    fn test_budget_for_context_window_scales_for_1m() {
        // 1M context → scale = 5.0, capped at 4.0
        let budget = TokenBudget::for_context_window(1_000_000);
        // max_total = 120_000 × 4 = 480_000
        assert_eq!(budget.max_total, 480_000);
        // Memory: 6_000 × 4 = 24_000
        assert_eq!(budget.limit_for(ContextSource::Memory), Some(24_000));
        // History: 50_000 × 4 = 200_000
        assert_eq!(budget.limit_for(ContextSource::History), Some(200_000));
    }

    #[test]
    fn test_budget_for_context_window_scales_for_400k() {
        // 400K context → scale = 2.0
        let budget = TokenBudget::for_context_window(400_000);
        assert_eq!(budget.max_total, 240_000);
        assert_eq!(budget.limit_for(ContextSource::Memory), Some(12_000));
    }
}
