//! A working budget, not a claim about the provider's maximum context window.
use serde_json::Value;

pub const WINDOW: u64 = 258_000;
pub const COMPACT_AT: u64 = 230_000;

#[derive(Default)]
pub struct Context {
    tokens: u64,
    covered_items: usize,
    includes_tools: bool,
}

impl Context {
    pub fn estimate(&self, history: &[Value], tools: &[Value]) -> u64 {
        let tokens = if self.covered_items == 0 {
            estimate(history)
        } else {
            self.tokens
                .saturating_add(estimate(&history[self.covered_items..]))
        };
        tokens.saturating_add(if self.includes_tools {
            0
        } else {
            estimate(tools)
        })
    }

    pub const fn record(&mut self, usage: Option<&crate::model::Usage>, items: usize) {
        if let Some(usage) = usage {
            self.tokens = usage.input_tokens.saturating_add(usage.output_tokens);
            self.covered_items = items;
            self.includes_tools = true;
        }
    }

    pub fn compacted(&mut self) {
        // Inline usage includes the pre-compaction input. It is not the new window's
        // size. Estimate retained items until the next ordinary response reports usage.
        *self = Self::default();
    }
}

fn estimate(items: &[Value]) -> u64 {
    // Approximation for newly added data. Provider usage replaces this after each reply.
    items
        .iter()
        .map(|item| (item.to_string().len() as u64).div_ceil(3) + 8)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Usage;
    use serde_json::json;

    #[test]
    fn usage_replaces_estimates_and_new_items_grow_the_budget() {
        let tools = [json!({"name":"shell"})];
        let mut history = vec![json!({"role":"user", "content":"hello"})];
        let mut context = Context::default();
        assert!(context.estimate(&history, &tools) > 0);
        context.record(
            Some(&Usage {
                input_tokens: 100,
                output_tokens: 20,
            }),
            history.len(),
        );
        assert_eq!(context.estimate(&history, &tools), 120);
        history.push(json!({"type":"function_call_output", "output":"result"}));
        let grown = context.estimate(&history, &tools);
        assert!(grown > 120);
        context.record(None, history.len());
        assert_eq!(context.estimate(&history, &tools), grown);
        context.compacted();
        assert_eq!(
            context.estimate(&history, &tools),
            estimate(&history) + estimate(&tools)
        );
    }
}
