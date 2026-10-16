use super::{AgentOptions, Core, CoreError, RuntimeProvider};
use carillon_protocol::model::ModelQuery;

pub(super) enum Owner<'a> {
    Current(&'a AgentOptions),
    Stranded {
        runtime_id: String,
        current: Option<&'a AgentOptions>,
    },
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
            return Ok(Owner::Stranded {
                runtime_id: id,
                current: agent.ok(),
            });
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
            Owner::Stranded {
                runtime_id,
                current,
            } => Err(CoreError::Conflict(stranded(
                &runtime_id,
                &query.agent_id,
                current.map(|agent| agent.runtime.as_str()),
            ))),
        }
    }
}

pub(super) fn stranded(runtime_id: &str, agent_id: &str, current: Option<&str>) -> String {
    let explanation =
        format!("This conversation ran on {runtime_id}, which no longer runs {agent_id}.");
    if let Some(current) = current {
        return format!("{explanation} Your next message starts a new conversation on {current}.");
    }
    explanation
}
