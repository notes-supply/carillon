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
5. Route each channel to an agent: Carillon's assistant, on Pi or on T3
   with Carillon's own profiles, or plain T3 Code in a project.
6. Choose models and their settings for a conversation, or an agent's
   default for new conversations, with `/model`, and run the runtime's own
   commands with `/command`.
7. Change users, routes, runtime availability and concurrency while Carillon
   runs; other settings apply on restart.
8. Replace the Discord module (slash commands and cards) without restarting.
9. Restart and update without interrupting running work, inspect recent
   turns, and back up and restore the installation.

## Providers

An **interface provider** owns a platform connection: its credentials,
admission, routing, commands and delivery. A **runtime provider** turns
Carillon turns into a native agent engine and owns its sessions, models,
tools and settings. Core keeps only what they share: admission, conversation
identity, turn state, the runtime that owns each conversation, and delivery.

A conversation belongs to the agent of the route it began in, and is
pinned to the runtime that took its first turn. Changing an agent's runtime
starts new conversations on the new runtime. Its old conversations end
with an explanation that asks the user to start a new conversation. Removing
an agent ends its conversations the same way. Messages there receive a
receipt and the explanation even if the former runtime is disabled or
removed. An otherwise current conversation whose runtime is unavailable
refuses new work. Nothing translates native state between engines.

## Behaviour

- **Admission.** Discord refuses direct messages and anything outside its
  guild, allowlist and routes before core sees it. Repeating the same event
  never starts a second turn.
- **Files.** A message is accepted with all its files or not at all. Files
  are kept with the turn and checked before it runs; nothing is extracted or
  executed on arrival.
- **Turns.** Core stores each accepted turn before acknowledging it, runs it
  once on its runtime, and stores the answer and its delivery together. A
  turn whose outcome can't be proved is marked interrupted and never
  replayed; the conversation is told, and the next message carries on with
  the same native session. Delivery retries without rerunning the model.
- **Visibility.** While a turn runs, a card shows its current tool calls,
  recent ones and delegated work, never tool output or private reasoning.
  It stays as a summary when the work ends.
- **Model selection.** `/model` stays within the conversation's runtime. A
  scope overrides the whole selection; returning to the default clears it.
  Queued work runs with the selection in force when it starts.
- **Authority.** Every service runs as the installing user. Carillon adds no
  command broker, path allowlist or approval system; confinement, if wanted,
  belongs to the runtime or its extensions.
- **Beta formats.** State and configuration use the current format only.
  When a format changes, an update refuses before stopping anything; there are
  no migrations.
