//! Guided Discord setup: the bot token, read without echo and written only
//! to the installation's credential file; the bot and its application,
//! learned from that token; and the server, channels and people, offered by
//! name and written as the provider's admission settings. Nobody is admitted
//! who was not named here.

use anyhow::{Context as _, Result};
use carillon_installation::{
    config::LoadedConfig, filesystem::atomic_write, maintenance::MaintenanceLock, prompt::Questions,
};
use twilight_http::{Client as DiscordHttp, error::ErrorType};
use twilight_model::{
    channel::{Channel, ChannelType},
    guild::Permissions,
    id::{
        Id,
        marker::{GuildMarker, UserMarker},
    },
    oauth::{Application, ApplicationFlags, team::TeamMembershipState},
    user::{CurrentUser, User},
};

use crate::config::{
    DiscordActivation, DiscordConfig, DiscordConversation, DiscordRoute, configure,
};

/// The bot's credential, beside the installation's others.
const TOKEN: &str = "discord-bot";

/// What the bot needs in its channels: to read and answer messages, start
/// and answer in threads, react, and show cards and files.
const PERMISSIONS: Permissions = Permissions::VIEW_CHANNEL
    .union(Permissions::SEND_MESSAGES)
    .union(Permissions::SEND_MESSAGES_IN_THREADS)
    .union(Permissions::CREATE_PUBLIC_THREADS)
    .union(Permissions::READ_MESSAGE_HISTORY)
    .union(Permissions::ADD_REACTIONS)
    .union(Permissions::EMBED_LINKS)
    .union(Permissions::ATTACH_FILES);

/// When the agent answers in a routed channel.
const MODES: [(&str, DiscordActivation, DiscordConversation); 4] = [
    (
        "Every message, each in its own thread",
        DiscordActivation::Always,
        DiscordConversation::Thread,
    ),
    (
        "Every message, the channel as one conversation",
        DiscordActivation::Always,
        DiscordConversation::Channel,
    ),
    (
        "Only when mentioned, each in its own thread",
        DiscordActivation::Mention,
        DiscordConversation::Thread,
    ),
    (
        "Only when mentioned, the channel as one conversation",
        DiscordActivation::Mention,
        DiscordConversation::Channel,
    ),
];

/// Asks for the bot token, then the server, channels and people, and writes
/// the token and the provider's settings. `connect` gives a Discord client
/// for a token. Keeping saved settings changes nothing.
///
/// # Errors
///
/// Returns an error when the input ends, Discord cannot be reached, or the
/// files cannot be written; nothing is written before every answer is in.
pub(crate) async fn setup(
    prompt: &mut impl Questions,
    loaded: &LoadedConfig,
    provider: &str,
    connect: impl Fn(String) -> DiscordHttp,
) -> Result<()> {
    let saved = loaded
        .credential(TOKEN)
        .ok()
        .filter(|token| !token.is_empty());
    if saved.is_some()
        && DiscordConfig::load(loaded, provider).is_ok()
        && prompt.confirm("Keep the saved Discord bot and settings?", true)?
    {
        return Ok(());
    }
    prompt.say(
        "Paste the bot token from the Bot page of your application in the Discord Developer \
         Portal. It is not shown as you type.",
    )?;
    let (token, http, bot) = sign_in(prompt, saved, &connect).await?;
    let application = message_content(prompt, &http).await?;
    prompt.say(&format!("Signed in as {}.", bot.name))?;
    let (guild_id, guild) = server(prompt, &http, &application).await?;
    let channels = channels(prompt, &http, guild_id, &guild).await?;
    let mode = prompt.choose(
        "When should the agent answer?",
        &MODES.map(|(label, _, _)| label.to_owned()),
        0,
    )?;
    let (_, activation, conversation) = MODES[mode];
    let people = people(prompt, &http, guild_id, &guild, &application, &bot).await?;

    let routes: Vec<DiscordRoute> = channels
        .iter()
        .map(|(id, _)| DiscordRoute {
            channel_id: id.clone(),
            activation,
            conversation,
        })
        .collect();
    let users: Vec<String> = people.iter().map(|(id, _)| id.clone()).collect();
    // Only the writes hold the maintenance lock, never the questions; the
    // settings file is reread under it, keeping what was tuned meanwhile.
    let maintenance = MaintenanceLock::acquire_unless_held(&loaded.root)
        .context("the bot token and Discord settings were not changed")?;
    let path = loaded
        .root
        .join("credentials")
        .join(format!("{TOKEN}.token"));
    atomic_write(&path, format!("{token}\n").as_bytes(), 0o600)
        .with_context(|| format!("write {}", path.display()))?;
    configure(
        loaded,
        provider,
        &guild_id.to_string(),
        &bot.id.to_string(),
        &users,
        &routes,
    )?;
    drop(maintenance);
    prompt.say(&format!(
        "{} answers {} in {} on {guild}: {}.",
        bot.name,
        people
            .iter()
            .map(|(_, name)| name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        channels
            .iter()
            .map(|(_, name)| format!("#{name}"))
            .collect::<Vec<_>>()
            .join(", "),
        MODES[mode].0.to_lowercase()
    ))?;
    Ok(())
}

/// The bot token, asked until Discord accepts it, with its client and user.
async fn sign_in(
    prompt: &mut impl Questions,
    saved: Option<String>,
    connect: &impl Fn(String) -> DiscordHttp,
) -> Result<(String, DiscordHttp, CurrentUser)> {
    let question = if saved.is_some() {
        "Bot token (Enter keeps the saved one):"
    } else {
        "Bot token:"
    };
    loop {
        let typed = prompt.secret(question)?;
        let token = match (typed.is_empty(), &saved) {
            (false, _) => typed,
            (true, Some(saved)) => saved.clone(),
            (true, None) => {
                prompt.say("The bot token is required.")?;
                continue;
            }
        };
        let http = connect(token.clone());
        match http.current_user().await {
            Ok(response) => {
                let bot = response.model().await.context("read the bot's user")?;
                return Ok((token, http, bot));
            }
            Err(failure) if refused(&failure) => prompt.say(
                "Discord refused this token. Copy it again from the Bot page, or reset it there \
                 and paste the new one.",
            )?,
            Err(failure) => {
                return Err(anyhow::Error::from(failure).context("ask Discord who the bot is"));
            }
        }
    }
}

fn refused(failure: &twilight_http::Error) -> bool {
    match failure.kind() {
        ErrorType::Response { status, .. } => status.get() == 401,
        ErrorType::Unauthorized => true,
        _ => false,
    }
}

/// The bot's application once Discord lets it read messages, which the
/// provider's gateway asks for.
async fn message_content(prompt: &mut impl Questions, http: &DiscordHttp) -> Result<Application> {
    loop {
        let application = http
            .current_user_application()
            .await?
            .model()
            .await
            .context("read the bot's application")?;
        let allowed = ApplicationFlags::GATEWAY_MESSAGE_CONTENT
            .union(ApplicationFlags::GATEWAY_MESSAGE_CONTENT_LIMITED);
        if application
            .flags
            .is_some_and(|flags| flags.intersects(allowed))
        {
            return Ok(application);
        }
        prompt.say(
            "Turn on Message Content Intent on the Bot page of the Developer Portal: the agent \
             reads the messages it answers.",
        )?;
        prompt.answer("Press Enter once it is on.", Some(""))?;
    }
}

/// The server the agent serves, chosen by name from the bot's servers, once
/// it is in one.
async fn server(
    prompt: &mut impl Questions,
    http: &DiscordHttp,
    application: &Application,
) -> Result<(Id<GuildMarker>, String)> {
    loop {
        let mut guilds = http.current_user_guilds().await?.model().await?;
        match guilds.len() {
            0 => {
                prompt.say(&format!(
                    "The bot is in no server yet. Invite it with this link:\n  \
                     https://discord.com/oauth2/authorize?client_id={}&scope=bot%20applications.commands&permissions={}",
                    application.id,
                    PERMISSIONS.bits()
                ))?;
                prompt.answer("Press Enter once it has joined.", Some(""))?;
            }
            1 => {
                let guild = guilds.remove(0);
                prompt.say(&format!("Server: {}", guild.name))?;
                return Ok((guild.id, guild.name));
            }
            _ => {
                let names: Vec<String> = guilds.iter().map(|guild| guild.name.clone()).collect();
                let index = prompt.choose("Which server should the agent serve?", &names, 0)?;
                let guild = guilds.swap_remove(index);
                return Ok((guild.id, guild.name));
            }
        }
    }
}

/// The text channels the agent answers in, by ID and name.
async fn channels(
    prompt: &mut impl Questions,
    http: &DiscordHttp,
    guild_id: Id<GuildMarker>,
    guild: &str,
) -> Result<Vec<(String, String)>> {
    loop {
        let mut text: Vec<Channel> = http
            .guild_channels(guild_id)
            .await?
            .model()
            .await?
            .into_iter()
            .filter(|channel| channel.kind == ChannelType::GuildText)
            .collect();
        text.sort_by_key(|channel| channel.position);
        if text.is_empty() {
            prompt.say(&format!(
                "The bot sees no text channels in {guild}. Give it access to one, or create one."
            ))?;
            prompt.answer("Press Enter to look again.", Some(""))?;
            continue;
        }
        let named: Vec<(String, String)> = text
            .into_iter()
            .map(|channel| (channel.id.to_string(), channel.name.unwrap_or_default()))
            .collect();
        let labels: Vec<String> = named.iter().map(|(_, name)| format!("#{name}")).collect();
        loop {
            let chosen = prompt.choose_many("Channels the agent answers in", &labels, &[0])?;
            if !chosen.is_empty() {
                return Ok(chosen
                    .into_iter()
                    .map(|index| named[index].clone())
                    .collect());
            }
            prompt.say("Choose at least one channel.")?;
        }
    }
}

/// The people the agent answers, by ID and name: the application's owners,
/// offered by name, and anyone else by Discord user ID, each a member of
/// the server.
async fn people(
    prompt: &mut impl Questions,
    http: &DiscordHttp,
    guild_id: Id<GuildMarker>,
    guild: &str,
    application: &Application,
    bot: &CurrentUser,
) -> Result<Vec<(String, String)>> {
    let owners: Vec<&User> = match &application.team {
        Some(team) => team
            .members
            .iter()
            .filter(|member| member.membership_state == TeamMembershipState::Accepted)
            .map(|member| &member.user)
            .collect::<Vec<&User>>(),
        None => application.owner.iter().collect(),
    }
    .into_iter()
    .filter(|user| !user.bot && user.id != bot.id)
    .collect();
    prompt.say("Only the people named here can talk to the agent.")?;
    let labels: Vec<String> = owners
        .iter()
        .map(|owner| {
            format!(
                "{} (owns the bot's application)",
                owner.global_name.as_deref().unwrap_or(&owner.name)
            )
        })
        .collect();
    // Every retry asks again from the last answers, so any of them can be
    // corrected, an owner who is not in the server included.
    let mut chosen: Vec<usize> = (0..owners.len()).collect();
    'ask: loop {
        if !owners.is_empty() {
            chosen = prompt.choose_many("People the agent answers", &labels, &chosen)?;
        }
        let others = prompt.answer(
            "Anyone else, by Discord user ID (separated by spaces, Enter for nobody):",
            Some(""),
        )?;
        let mut ids: Vec<Id<UserMarker>> = chosen.iter().map(|&index| owners[index].id).collect();
        for word in others.split_whitespace() {
            let Ok(id) = word.parse() else {
                prompt.say(&format!("{word} is not a Discord user ID."))?;
                continue 'ask;
            };
            ids.push(id);
        }
        if ids.is_empty() {
            prompt.say("Name at least one person.")?;
            continue;
        }
        let mut people: Vec<(String, String)> = Vec::new();
        for id in ids {
            if people.iter().any(|(known, _)| *known == id.to_string()) {
                continue;
            }
            match http.guild_member(guild_id, id).await {
                Ok(response) => {
                    let member = response.model().await?;
                    let name = member
                        .nick
                        .or(member.user.global_name)
                        .unwrap_or(member.user.name);
                    people.push((id.to_string(), name));
                }
                Err(failure) if matches!(failure.kind(), ErrorType::Response { status, .. } if status.get() == 404) =>
                {
                    let who = owners.iter().find(|owner| owner.id == id).map_or_else(
                        || id.to_string(),
                        |owner| {
                            format!(
                                "{} ({id})",
                                owner.global_name.as_deref().unwrap_or(&owner.name)
                            )
                        },
                    );
                    prompt.say(&format!("{who} is not a member of {guild}."))?;
                    continue 'ask;
                }
                Err(failure) => {
                    return Err(anyhow::Error::from(failure).context("look up a server member"));
                }
            }
        }
        return Ok(people);
    }
}

#[cfg(test)]
mod tests;
