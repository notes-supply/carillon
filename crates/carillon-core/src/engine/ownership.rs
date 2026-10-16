use super::{AgentOptions, Core, CoreError, RuntimeProvider};
use carillon_protocol::model::ModelQuery;

pub(super) enum Owner<'a> {
    Current(&'a AgentOptions),
    Stranded(String),
}

impl Core {
    pub(super) fn conversation_owner(
        &self,
        interface_id: &str,
        query: &ModelQuery,
    ) -> Result<Owner<'_>, CoreError> {
        let agent = self.agent(&query.agent_id);
        if let Some(audience) = &query.audience_id
            && let Some(id) =
                self.database
                    .conversation_runtime(interface_id, &query.agent_id, audience)?
            && !agent.as_ref().is_ok_and(|agent| id == agent.runtime)
        {
            return Ok(Owner::Stranded(id));
        }
        Ok(Owner::Current(agent?))
    }

    /// Refuses an interactive request for a conversation whose agent moved or was removed.
    /// Messages instead commit their interruption notice during admission.
    /// # Errors
    /// Returns an unknown agent, stranded conversation or storage error.
    pub(crate) fn query_runtime(
        &self,
        interface_id: &str,
        query: &ModelQuery,
    ) -> Result<RuntimeProvider, CoreError> {
        match self.conversation_owner(interface_id, query)? {
            Owner::Current(agent) => self.enabled_runtime(&agent.runtime),
            Owner::Stranded(id) => Err(CoreError::Conflict(stranded(&id, &query.agent_id))),
        }
    }
}

pub(super) fn stranded(runtime_id: &str, agent_id: &str) -> String {
    format!(
        "This conversation ran on {runtime_id}, which no longer runs {agent_id}. Start a new conversation."
    )
}
