//! Message operations, in the guild's channels and in its allowlisted
//! people's direct messages. What a send makes is recorded: the agent that
//! sent it, and a direct message for the person's next one to quote.
use serde_json::{Value, json};
use twilight_http::{Client, request::channel::reaction::RequestReactionType};
use twilight_model::channel::message::AllowedMentions;

use super::{Messages, OperationError, Scope, decoded, direct, uploads, validate_limit};
use crate::conversations::{Participation, SentDirect};

impl Messages {
    #[expect(
        clippy::too_many_lines,
        reason = "direct dispatch over the resource-specific Discord operations"
    )]
    pub(super) async fn run(
        self,
        http: &Client,
        scope: &Scope,
        participation: &Participation,
    ) -> Result<Value, OperationError> {
        match self {
            Self::List {
                channel_id,
                user_id,
                before,
                limit,
            } => {
                let channel_id = direct::destination(http, scope, channel_id, user_id)
                    .await?
                    .id;
                validate_limit(limit)?;
                let request = http.channel_messages(channel_id);
                let response = if let Some(before) = before {
                    request.before(before).limit(limit).await?
                } else {
                    request.limit(limit).await?
                };
                let messages = response
                    .models()
                    .await
                    .map_err(|_| OperationError::uncertain())?;
                let next = if messages.len() == usize::from(limit) {
                    messages.last().map(|m| m.id)
                } else {
                    None
                };
                Ok(
                    json!({ "items": messages, "next_before": next, "may_have_more": next.is_some() }),
                )
            }
            Self::Get {
                channel_id,
                message_id,
            } => {
                direct::conversation(http, scope, channel_id).await?;
                decoded(http.message(channel_id, message_id).await?).await
            }
            Self::Send {
                channel_id,
                user_id,
                content,
                reply_to,
                mention_users,
                mention_roles,
                attachments,
                sender,
                ..
            } => {
                let attachments = uploads(attachments)?;
                if content.is_empty() && attachments.is_empty() {
                    return Err(OperationError::invalid(
                        "A message needs content or attachments.",
                    ));
                }
                let target = direct::destination(http, scope, channel_id, user_id).await?;
                let channel_id = target.id;
                if let Some(message) = reply_to {
                    http.message(channel_id, message).await?;
                }
                let mentions = AllowedMentions {
                    users: mention_users,
                    roles: mention_roles,
                    ..AllowedMentions::default()
                };
                let mut request = http
                    .create_message(channel_id)
                    .content(&content)
                    .allowed_mentions(Some(&mentions));
                if !attachments.is_empty() {
                    request = request.attachments(&attachments);
                }
                if let Some(message) = reply_to {
                    request = request.reply(message);
                }
                let message = request
                    .await?
                    .model()
                    .await
                    .map_err(|_| OperationError::uncertain())?;
                participation.record(|state| {
                    if let Some(sender) = &sender {
                        state.sent_by([message.id], sender);
                    }
                    if let Some(person) = target.quoting(sender.as_ref()) {
                        state.sent_directly(&person.to_string(), SentDirect::new(&message, sender));
                    }
                });
                let mut result =
                    serde_json::to_value(&message).map_err(|_| OperationError::uncertain())?;
                result["url"] = json!(target.link(scope, &message.id.to_string()));
                Ok(result)
            }
            Self::Edit {
                channel_id,
                message_id,
                content,
            } => {
                direct::conversation(http, scope, channel_id).await?;
                decoded(
                    http.update_message(channel_id, message_id)
                        .content(Some(&content))
                        .allowed_mentions(Some(&AllowedMentions::default()))
                        .await?,
                )
                .await
            }
            Self::Delete {
                channel_id,
                message_id,
            } => {
                direct::conversation(http, scope, channel_id).await?;
                http.delete_message(channel_id, message_id).await?;
                Ok(json!({ "message_id": message_id, "deleted": true }))
            }
            Self::Pin {
                channel_id,
                message_id,
                pinned,
            } => {
                direct::conversation(http, scope, channel_id).await?;
                if pinned {
                    http.create_pin(channel_id, message_id).await?;
                } else {
                    http.delete_pin(channel_id, message_id).await?;
                }
                Ok(json!({ "message_id": message_id, "pinned": pinned }))
            }
            Self::React {
                channel_id,
                message_id,
                emoji,
                add,
            } => {
                direct::conversation(http, scope, channel_id).await?;
                if emojis::get(&emoji).is_none() {
                    return Err(OperationError::invalid(
                        "Use a Unicode emoji for reactions.",
                    ));
                }
                let emoji = RequestReactionType::Unicode { name: &emoji };
                if add {
                    http.create_reaction(channel_id, message_id, &emoji).await?;
                } else {
                    http.delete_current_user_reaction(channel_id, message_id, &emoji)
                        .await?;
                }
                Ok(json!({ "message_id": message_id, "added": add }))
            }
        }
    }
}
