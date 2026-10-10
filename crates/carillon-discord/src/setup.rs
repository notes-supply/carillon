//! Guided Discord setup: the bot token, read without echo and written only
//! to the installation's credential file; the bot and its application,
//! learned from that token; and the server, channels and people, offered by
//! name and written as the provider's admission settings. Nobody is admitted
//! who was not named here, and no channel is routed where the bot may not
//! answer.

use std::fmt::Display;

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
        marker::{ChannelMarker, GuildMarker, RoleMarker, UserMarker},
    },
    oauth::{Application, ApplicationFlags, team::TeamMembershipState},
    user::{CurrentUser, User},
};
use twilight_util::permission_calculator::PermissionCalculator;

use crate::config::{
    DiscordActivation, DiscordConfig, DiscordConversation, DiscordRoute, configure,
};

/// The bot's credential, beside the installation's others.
const TOKEN: &str = "discord-bot";

/// Why the bot asks for a permission in its channels.
#[derive(Clone, Copy)]
enum Need {
    /// To read and answer messages.
    Answer,
    /// To start and answer in a thread per conversation.
    Threads,
    /// To show what it names; without it the agent answers all the same.
    Show(&'static str),
}

/// What the bot asks for in its channels, by the names Discord's settings
/// give them.
const PERMISSIONS: [(Permissions, &str, Need); 8] = [
    (Permissions::VIEW_CHANNEL, "View Channel", Need::Answer),
    (Permissions::SEND_MESSAGES, "Send Messages", Need::Answer),
    (
        Permissions::READ_MESSAGE_HISTORY,
        "Read Message History",
        Need::Answer,
    ),
    (
        Permissions::CREATE_PUBLIC_THREADS,
        "Create Public Threads",
        Need::Threads,
    ),
    (
        Permissions::SEND_MESSAGES_IN_THREADS,
        "Send Messages in Threads",
        Need::Threads,
    ),
    (Permissions::EMBED_LINKS, "Embed Links", Need::Show("cards")),
    (
        Permissions::ADD_REACTIONS,
        "Add Reactions",
        Need::Show("reactions"),
    ),
    (
        Permissions::ATTACH_FILES,
        "Attach Files",
        Need::Show("files"),
    ),
];

/// The bot token setup leaves for the provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Token {
    /// The one the provider has, if any: the settings apply live.
    Unchanged,
    /// A new one, which the provider reads only when it starts.
    Replaced,
}

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
) -> Result<Token> {
    let saved = loaded
        .credential(TOKEN)
        .ok()
        .filter(|token| !token.is_empty());
    if saved.is_some()
        && DiscordConfig::load(loaded, provider).is_ok()
        && prompt.confirm("Keep the saved Discord bot and settings?", true)?
    {
        return Ok(Token::Unchanged);
    }
    prompt.say(
        "Paste the bot token from the Bot page of your application in the Discord Developer \
         Portal. It is not shown as you type.",
    )?;
    let (token, http, bot) = sign_in(prompt, saved.as_deref(), &connect).await?;
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
    permitted(prompt, &http, guild_id, bot.id, &channels, conversation).await?;
    let people = people(prompt, &http, guild_id, &guild, &application, &bot).await?;

    let routes: Vec<DiscordRoute> = channels
        .iter()
        .map(|(id, _)| DiscordRoute {
            channel_id: id.to_string(),
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
            .map(|(_, label)| label.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        MODES[mode].0.to_lowercase()
    ))?;
    Ok(if saved.is_some_and(|saved| saved != token) {
        Token::Replaced
    } else {
        Token::Unchanged
    })
}

/// The bot token, asked until Discord accepts it, with its client and user.
async fn sign_in(
    prompt: &mut impl Questions,
    saved: Option<&str>,
    connect: &impl Fn(String) -> DiscordHttp,
) -> Result<(String, DiscordHttp, CurrentUser)> {
    let question = if saved.is_some() {
        "Bot token (Enter keeps the saved one):"
    } else {
        "Bot token:"
    };
    loop {
        let typed = prompt.secret(question)?;
        let token = match (typed.is_empty(), saved) {
            (false, _) => typed,
            (true, Some(saved)) => saved.to_owned(),
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

/// The server the agent serves, by ID and label, chosen by name from the
/// bot's servers once it is in one.
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
                    PERMISSIONS
                        .iter()
                        .fold(Permissions::empty(), |all, (permission, ..)| all | *permission)
                        .bits()
                ))?;
                prompt.answer("Press Enter once it has joined.", Some(""))?;
            }
            1 => {
                let guild = guilds.remove(0);
                prompt.say(&format!("Server: {}", guild.name))?;
                return Ok((guild.id, guild.name));
            }
            _ => {
                let named: Vec<(Id<GuildMarker>, String)> = guilds
                    .into_iter()
                    .map(|guild| (guild.id, guild.name))
                    .collect();
                let mut labels = distinct(&named);
                let index = prompt.choose("Which server should the agent serve?", &labels, 0)?;
                return Ok((named[index].0, labels.swap_remove(index)));
            }
        }
    }
}

/// The text channels the agent answers in, by ID and label.
async fn channels(
    prompt: &mut impl Questions,
    http: &DiscordHttp,
    guild_id: Id<GuildMarker>,
    guild: &str,
) -> Result<Vec<(Id<ChannelMarker>, String)>> {
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
        let named: Vec<(Id<ChannelMarker>, String)> = text
            .into_iter()
            .map(|channel| (channel.id, format!("#{}", channel.name.unwrap_or_default())))
            .collect();
        let labels = distinct(&named);
        loop {
            let chosen = prompt.choose_many("Channels the agent answers in", &labels, &[0])?;
            if !chosen.is_empty() {
                return Ok(chosen
                    .into_iter()
                    .map(|index| (named[index].0, labels[index].clone()))
                    .collect());
            }
            prompt.say("Choose at least one channel.")?;
        }
    }
}

/// Each name, followed by its ID where another has the same name.
fn distinct<T: Display>(named: &[(T, String)]) -> Vec<String> {
    named
        .iter()
        .map(|(id, name)| {
            if named.iter().filter(|(_, other)| other == name).count() > 1 {
                format!("{name} ({id})")
            } else {
                name.clone()
            }
        })
        .collect()
}

/// Waits until the bot may answer in every chosen channel the way
/// `conversation` needs, by its roles and each channel's overwrites as
/// Discord applies them, then says where the agent answers without cards,
/// reactions or files. Each check reads them from Discord again.
async fn permitted(
    prompt: &mut impl Questions,
    http: &DiscordHttp,
    guild_id: Id<GuildMarker>,
    bot: Id<UserMarker>,
    chosen: &[(Id<ChannelMarker>, String)],
    conversation: DiscordConversation,
) -> Result<()> {
    loop {
        let roles = http.roles(guild_id).await?.model().await?;
        let member = http.guild_member(guild_id, bot).await?.model().await?;
        let channels = http.guild_channels(guild_id).await?.model().await?;
        let everyone = roles
            .iter()
            .find(|role| role.id.get() == guild_id.get())
            .map_or(Permissions::empty(), |role| role.permissions);
        let bot_roles: Vec<(Id<RoleMarker>, Permissions)> = roles
            .iter()
            .filter(|role| member.roles.contains(&role.id))
            .map(|role| (role.id, role.permissions))
            .collect();
        let mut unusable = Vec::new();
        let mut unshown = Vec::new();
        for (id, label) in chosen {
            let channel = channels
                .iter()
                .find(|channel| channel.id == *id)
                .with_context(|| format!("{label} is gone from the server; run setup again"))?;
            let held = PermissionCalculator::new(guild_id, bot, everyone, &bot_roles).in_channel(
                channel.kind,
                channel.permission_overwrites.as_deref().unwrap_or_default(),
            );
            let mut lacking = Vec::new();
            let mut hidden = Vec::new();
            for &(permission, name, need) in &PERMISSIONS {
                if held.contains(permission) {
                    continue;
                }
                match need {
                    Need::Answer => lacking.push(name),
                    Need::Threads if conversation == DiscordConversation::Thread => {
                        lacking.push(name);
                    }
                    Need::Threads => {}
                    Need::Show(what) => hidden.push((name, what)),
                }
            }
            if !lacking.is_empty() {
                unusable.push(format!(
                    "The bot cannot answer in {label}: allow it {} in the channel's settings.",
                    lacking.join(", ")
                ));
            } else if !hidden.is_empty() {
                unshown.push(format!(
                    "In {label} the bot lacks {}, so the agent answers there without {}.",
                    hidden
                        .iter()
                        .map(|(name, _)| *name)
                        .collect::<Vec<_>>()
                        .join(", "),
                    hidden
                        .iter()
                        .map(|(_, what)| *what)
                        .collect::<Vec<_>>()
                        .join(" or ")
                ));
            }
        }
        if unusable.is_empty() {
            for note in unshown {
                prompt.say(&note)?;
            }
            return Ok(());
        }
        for problem in unusable {
            prompt.say(&problem)?;
        }
        prompt.answer("Press Enter to check again.", Some(""))?;
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
