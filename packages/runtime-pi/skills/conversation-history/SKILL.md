---
name: conversation-history
description: Read your own past conversations and model usage. Use when asked what was discussed before, in a channel, thread, period or about a topic, or how many tokens have been used.
---

# Conversation history

Pi records every conversation you hold in one SQLite store, the file your prompt names in `<conversation_store>`. The runtime writes to it while you read, so always open it read-only, through this skill's views:

```sh
sqlite3 -init "<this skill's directory>/views.sql" -box "file:<store>?mode=ro" "<query>"
```

## Views

- `conversation_list`: one row per conversation with people. `conversation` is its ID. `interface`, `guild` and `channel` say where it is held. `thread_of` is the parent channel when the conversation is a Discord thread. `first` and `last` are when it began and last spoke, `messages` counts the person's messages, `subagents` counts the subagent runs it started, and `opening` is how the person's first message began, without Carillon's notes or quotes. Every view counts a subagent run under the conversation that started it.
- `messages`: what the person and you said, without thinking or tool calls. Columns `conversation`, `subagent`, `entry`, `position` (its place in the conversation, from 1), `at`, `role` (`user` or `assistant`) and `text`. The person's text includes Carillon's notes, which start `[Carillon`. A subagent run's messages have its ID in `subagent`, and their `user` text is the task it was given. They take positions in the conversation in the order they were written.
- `usage`: the tokens pi-durable counted, with `conversation`, `at`, `kind`, `model`, `input`, `output` (which includes `reasoning`), `cache_read`, `cache_write`, `total` and `estimated_cost`. A `response` row is one model response. pi-durable counts compaction summaries and tools' own model calls without timing them, so the conversation and each of its subagent runs have one `compaction` row per model and one `tool` row per tool, with the tool in `model`, dated at that run's latest compaction or else its last entry. Sum them for a conversation's figure. Totals by conversation and model are exact; by day, a conversation's summaries all fall on the day of its latest compaction.

Every `at`, `first` and `last` is local time as `YYYY-MM-DD HH:MM:SS`, so compare it with `datetime('now', 'localtime', '-7 days')` or `date('now', 'localtime', 'weekday 0', '-6 days')` for the week since Monday.

## Queries

Search with this query first. It shows each message containing the term, marked `>` in `hit`, with one message either side, latest conversation first. For more context, widen both ends of its frame.

```sql
WITH search(term) AS (VALUES ('invoice')),
found AS (SELECT messages.*, instr(lower(text), lower(term)) AS hit FROM messages, search),
near AS (
  SELECT *, max(hit > 0) OVER (PARTITION BY conversation ORDER BY position
      ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) AS shown,
    max(CASE WHEN hit THEN entry END) OVER (PARTITION BY conversation) AS latest
  FROM found
)
SELECT conversation, position, at, subagent, role, CASE WHEN hit THEN '>' ELSE '' END AS hit,
  replace(substr(text, max(1, hit - 60), 200), char(10), ' ') AS text
FROM near WHERE shown ORDER BY latest DESC, position LIMIT 40;
```

Read a whole conversation only when the matches are not enough, and a long one in ranges, with `AND position BETWEEN 10 AND 40`.

```sql
SELECT * FROM conversation_list ORDER BY last DESC LIMIT 20;

SELECT position, at, subagent, role, text FROM messages WHERE conversation = 4 ORDER BY position;

SELECT date(at) AS day, model, sum(input) AS input, sum(output) AS output,
  sum(cache_read) AS cache_read, sum(total) AS total,
  round(sum(estimated_cost), 2) AS estimated_usd
FROM usage WHERE at >= date('now', 'localtime', '-6 days')
GROUP BY day, model ORDER BY day;

SELECT conversation, channel, thread_of, sum(total) AS total
FROM usage JOIN conversation_list USING (conversation)
GROUP BY conversation ORDER BY total DESC;

SELECT CASE WHEN every THEN 'all' ELSE conversation END AS conversation,
  sum(kind = 'response') AS responses, sum(total) AS total,
  round(100.0 * sum(cache_read) / sum(input + cache_read + cache_write), 1) AS cache_hit_pct
FROM usage, (SELECT 0 AS every UNION ALL SELECT 1)
WHERE at >= date('now', 'localtime', '-6 days')
GROUP BY every, 1 ORDER BY every, total DESC;
```

## Answering

- Summarise what was said and when, and link a Discord conversation as `https://discord.com/channels/<guild>/<channel>`.
- Report usage in tokens. `estimated_cost` prices the tokens at the model catalogue's rates; on a subscription, such as ChatGPT's, nothing is billed per token. Call any dollar figure an estimate.
- Report `cache_hit_pct` as a percentage of the prompt tokens read from the provider's cache. It covers every counted model call: responses, compaction summaries and tools' own calls.

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
