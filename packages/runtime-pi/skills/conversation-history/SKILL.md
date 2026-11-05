---
name: conversation-history
description: Read your own past conversations and model usage. Use when asked what was discussed before, in a channel, thread, period or about a topic, or how many tokens have been used.
---

# Conversation history

Pi records every conversation you hold in one SQLite store, the file your prompt names in `<conversation_store>`. The runtime writes to it while you read, so always open it read-only, through this skill's views:

```sh
sqlite3 -cmd ".read <this skill's directory>/views.sql" -box "file:<store>?mode=ro" "<query>"
```

## Views

- `conversation_list`: one row per conversation. `conversation` is its ID. `interface`, `guild` and `channel` say where it is held. `thread_of` is the parent channel when the conversation is a Discord thread. `started_by` is the conversation that started a subagent's. `first` and `last` are when it began and last spoke, and `messages` counts the person's messages.
- `messages`: what the person and you said, without thinking or tool calls. Columns `conversation`, `entry` (its order), `at`, `role` (`user` or `assistant`) and `text`. The person's text includes Carillon's notes, which start `[Carillon`.
- `usage`: one row per model response, with `conversation`, `at`, `model`, `input`, `output` (which includes `reasoning`), `cache_read`, `cache_write`, `total` and `estimated_cost`.

Every `at`, `first` and `last` is local time as `YYYY-MM-DD HH:MM:SS`, so compare it with `datetime('now', 'localtime', '-7 days')` or `date('now', 'localtime', 'weekday 0', '-6 days')` for the week since Monday.

## Queries

```sql
SELECT * FROM conversation_list ORDER BY last DESC LIMIT 20;

SELECT at, role, text FROM messages WHERE conversation = 4 ORDER BY entry;

SELECT conversation, at, role, substr(text, 1, 300) AS text FROM messages
WHERE text LIKE '%invoice%' ORDER BY entry DESC LIMIT 20;

SELECT date(at) AS day, model, sum(input) AS input, sum(output) AS output,
  sum(cache_read) AS cache_read, sum(total) AS total,
  round(sum(estimated_cost), 2) AS estimated_usd
FROM usage WHERE at >= date('now', 'localtime', '-6 days')
GROUP BY day, model ORDER BY day;

SELECT conversation, channel, thread_of, sum(total) AS total
FROM usage JOIN conversation_list USING (conversation)
GROUP BY conversation ORDER BY total DESC;
```

Read a long conversation in pages, with `LIMIT` and `OFFSET`.

## Answering

- Summarise what was said and when, and link a Discord conversation as `https://discord.com/channels/<guild>/<channel>`.
- Report usage in tokens. `estimated_cost` prices the tokens at the model catalogue's rates; on a subscription, such as ChatGPT's, nothing is billed per token. Call any dollar figure an estimate.

## Without sqlite3

Carillon's runtime executable includes Bun and its SQLite. Run the same views and query through it:

```sh
BUN_BE_BUN=1 "$CARILLON_HOME/app/bin/carillon-runtime-pi" -e '
import { Database } from "bun:sqlite";
const [store, views, query] = process.argv.slice(1);
const db = new Database(store, { readonly: true });
db.exec(await Bun.file(views).text());
console.table(db.query(query).all());
' "<store>" "<this skill's directory>/views.sql" "<query>"
```
