use super::{Core, CoreError, RuntimeProvider};
use carillon_protocol::model::ModelQuery;

enum Owner<'a> {
    Current(&'a str),
    Stranded(String),
}

impl Core {
    fn conversation_owner(
        &self,
        interface_id: &str,
        query: &ModelQuery,
    ) -> Result<Owner<'_>, CoreError> {
        let agent = self.agent(&query.agent_id)?;
        if let Some(audience) = &query.audience_id
            && let Some(id) =
                self.database
                    .conversation_runtime(interface_id, &query.agent_id, audience)?
            && id != agent.runtime
        {
            return Ok(Owner::Stranded(id));
        }
        Ok(Owner::Current(&agent.runtime))
    }

    /// Resolve the pinned conversation owner, or the agent's runtime for a
    /// new conversation. Admission keeps a former owner so the worker can
    /// deliver an explained failure instead of rerouting the conversation.
    /// # Errors
    /// Returns an unknown agent, missing/disabled owner or storage error.
    pub fn runtime_for(
        &self,
        interface_id: &str,
        query: &ModelQuery,
    ) -> Result<RuntimeProvider, CoreError> {
        match self.conversation_owner(interface_id, query)? {
            Owner::Current(id) => self.enabled_runtime(id),
            Owner::Stranded(id) => self.enabled_runtime(&id),
        }
    }

    /// Refuses an interactive request for a conversation whose agent changed runtimes.
    /// Messages are still admitted for the worker to deliver their interruption notice.
    /// # Errors
    /// Returns an unknown agent, stranded conversation or storage error.
    pub(crate) fn query_runtime(
        &self,
        interface_id: &str,
        query: &ModelQuery,
    ) -> Result<RuntimeProvider, CoreError> {
        match self.conversation_owner(interface_id, query)? {
            Owner::Current(id) => self.enabled_runtime(id),
            Owner::Stranded(id) => Err(CoreError::Conflict(stranded(&id, &query.agent_id))),
        }
    }
}

pub(super) fn stranded(runtime_id: &str, agent_id: &str) -> String {
    format!(
        "This conversation ran on {runtime_id}, which no longer runs {agent_id}. Start a new conversation."
    )
}
