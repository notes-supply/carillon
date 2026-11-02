# Carillon product brief

Carillon connects messaging platforms to agent runtimes on one Linux host.
Discord is the first interface and Pi the default runtime, with T3 Code as an
alternative. Each is a separate program behind the `carillon` command, so a
new interface or runtime doesn't mean rebuilding the rest.

## What an owner can do

1. Install Carillon as an ordinary Unix user.
2. Configure a Discord server, allowlisted users and routed channels.
3. Talk to the agent in channels or threads, with files and images.
4. Let the agent use its runtime's native tools, extensions and skills with
   the owner's authority, and watch a running turn on a Discord card.
5. Route each channel to an agent: Carillon's assistant, on Pi, or plain T3
   Code in a project.
6. See and change a channel's model and its settings with `/settings` or
   `carillon settings`, set an agent's own model with `carillon settings
   --agent`, and run the runtime's own commands with `/command`.
7. Change users, routes and runtime availability while Carillon runs; other
   settings apply on restart.
8. Replace the Discord module (slash commands and cards) without restarting.
9. Restart and update without interrupting running work, inspect recent
   turns, and back up and restore the installation.

## Providers

An **interface provider** owns a platform connection: its credentials,
admission, routing, commands and delivery. A **runtime provider** turns
Carillon turns into a native agent engine and owns its sessions, models,
tools and settings. Core keeps only what they share: admission, conversation
identity, turn state, the runtime that owns each conversation, and delivery.

A conversation belongs to the agent of the route it began in and the runtime
of its latest turn. After an agent moves runtimes, the first message in an old
conversation receives an interruption notice and transfers ownership to the
current runtime. The next message starts a fresh native conversation there,
including in a Discord channel that keeps one conversation. Removing an agent
leaves its conversations stranded. Messages there receive a receipt and an
explanation even if the former runtime is disabled or removed.
An otherwise current conversation whose runtime is unavailable
refuses new work. Changing an agent's T3 project also ends conversations that
already have T3 threads. Their messages, queued turns, reports of delegated
work, model requests, and commands receive an explanation asking for a new
conversation.
Work already running in the previous project continues there. Nothing translates
native state between engines or T3 projects.

## Behaviour

- **Admission.** Discord refuses direct messages and anything outside its
  guild, allowlist and routes before core sees it. Repeating the same event
  never starts a second turn.
- **Files.** A message is accepted with all its files or not at all. Files
  are kept with the turn and checked before it runs; nothing is extracted or
  executed on arrival.
- **Turns.** Core stores each accepted turn before acknowledging it, hands it
  once to its runtime, and stores the answer and its delivery together. Pi
  holds the turns it accepts and finishes them across its own restarts; a
  tool call a crash cut off is reported to the model as interrupted, never
  repeated. T3 holds its turns too: restarted, it follows each again in
  T3's records rather than running it twice, and a turn whose outcome T3
  can't show fails and is never replayed. Either way the conversation is
  told, and the next message carries on with the same native conversation.
  Delivery retries without rerunning the model.
- **Busy conversations.** A message that arrives while Pi answers in its
  conversation steers that answer by default, or waits for it or interrupts
  it, as the agent's settings choose. People can steer a waiting message in,
  interrupt for it, or stop it, with reactions. On T3, a conversation's
  messages wait their turn, and people can stop one, waiting or running.
- **Visibility.** While a turn runs, a card shows its current tool calls,
  recent ones and delegated work, never tool output or private reasoning.
  It stays as a summary when the work ends.
- **Settings.** Each setting has one owner. A channel's model comes from
  the runtime that serves its agent, and a channel without its own takes
  the agent's, which is the only default. A change drawn from a page that
  has changed since writes nothing.
- **Authority.** Every service runs as the installing user. Carillon adds no
  command broker, path allowlist or approval system; confinement, if wanted,
  belongs to the runtime or its extensions.
- **Beta formats.** State and configuration use the current format only.
  When a format changes, an update refuses before stopping anything; there are
  no migrations.
