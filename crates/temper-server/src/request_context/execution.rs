//! The inline steps and detached work an agent context admits.

use super::{AgentContext, CallbackBudgetExceeded, InlineChain, MAX_CALLBACK_HOPS};

impl AgentContext {
    /// Admit a callback running `action` on an entity, unless its inline
    /// chain has already taken that step or the lineage has used its hops.
    pub(crate) fn for_callback(
        &self,
        entity_type: &str,
        entity_id: &str,
        action: &str,
    ) -> Result<Self, CallbackBudgetExceeded> {
        let step = self.inline_chain.step(entity_type, entity_id, action);
        let inline_chain = self
            .inline_chain
            .then(step)
            .map_err(CallbackBudgetExceeded::RepeatedStep)?;
        if self.callback_hops >= MAX_CALLBACK_HOPS {
            return Err(CallbackBudgetExceeded::TotalHops);
        }
        let mut next = self.clone();
        next.inline_chain = inline_chain;
        next.callback_hops += 1;
        Ok(next)
    }

    /// This context inside `action` on an entity: the step of its inline
    /// chain that the action's own inline work continues.
    pub(crate) fn entering(&self, entity_type: &str, entity_id: &str, action: &str) -> Self {
        let mut next = self.clone();
        let step = self.inline_chain.step(entity_type, entity_id, action);
        next.inline_chain = self.inline_chain.entering(step);
        next
    }

    /// This context after its step made these entities (`(type, id)`), so its
    /// chain knows them as made rather than found.
    pub(crate) fn having_made<'a>(
        mut self,
        made: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Self {
        for (entity_type, entity_id) in made {
            self.inline_chain = self.inline_chain.making(entity_type, entity_id);
        }
        self
    }

    /// Begin a detached task without replenishing its causal callback budget.
    ///
    /// Call only at a runtime-owned spawn boundary: the parent inline chain
    /// ends there, but its callback lineage continues in the detached task.
    pub(crate) fn for_background_task(&self) -> Self {
        let mut next = self.clone();
        next.inline_chain = InlineChain::default();
        next
    }
}
