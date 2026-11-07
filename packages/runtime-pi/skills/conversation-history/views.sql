CREATE TEMP VIEW messages AS
SELECT *, row_number() OVER (PARTITION BY conversation ORDER BY entry) AS position FROM (
  SELECT coalesce(c.owner_conversation_id, c.id) AS conversation,
    CASE WHEN c.owner_conversation_id IS NOT NULL THEN c.id END AS subagent,
    e.id AS entry,
    datetime(json_extract(e.record, '$.model[0].timestamp') / 1000, 'unixepoch', 'localtime') AS at,
    json_extract(e.record, '$.model[0].role') AS role,
    CASE json_type(e.record, '$.model[0].content')
      WHEN 'text' THEN json_extract(e.record, '$.model[0].content')
      ELSE (SELECT group_concat(json_extract(part.value, '$.text'), char(10, 10))
        FROM json_each(e.record, '$.model[0].content') AS part
        WHERE json_extract(part.value, '$.type') = 'text')
    END AS text
  FROM entries AS e
  JOIN conversations AS c ON c.id = e.conversation_id
  WHERE json_extract(e.record, '$.kind') IN ('carillon.message', 'pi.user', 'pi.assistant')
)
WHERE text <> '';

CREATE TEMP VIEW usage AS
WITH responses AS (
  SELECT e.conversation_id AS run,
    e.id AS entry,
    json_extract(e.record, '$.model[0].timestamp') AS at,
    json_extract(e.record, '$.model[0].provider') || '/' || json_extract(e.record, '$.model[0].model') AS model,
    json_extract(e.record, '$.model[0].usage') AS counts
  FROM entries AS e
  WHERE json_extract(e.record, '$.kind') = 'pi.assistant'
),
calls AS (
  SELECT run, entry, at, 'response' AS kind, model, 1 AS sign, counts FROM responses
  UNION ALL
  SELECT doc.owner_id, NULL, NULL, CASE bucket.key WHEN 'models' THEN 'compaction' ELSE 'tool' END,
    item.key, 1, item.value
  FROM documents AS doc
  JOIN document_revisions AS revision ON revision.document_id = doc.id
    AND revision.seq = (SELECT max(seq) FROM document_revisions WHERE document_id = doc.id)
  JOIN json_each(revision.content) AS bucket
  JOIN json_each(bucket.value) AS item
  WHERE json_extract(doc.record, '$.kind') = 'pi.usage'
  UNION ALL
  SELECT run, NULL, NULL, 'compaction', model, -1, counts FROM responses
)
SELECT coalesce(c.owner_conversation_id, c.id) AS conversation,
  datetime(coalesce(max(calls.at),
    (SELECT max(json_extract(e.record, '$.model[0].timestamp')) FROM entries AS e
      WHERE e.conversation_id = calls.run AND json_extract(e.record, '$.kind') = 'pi.compaction'),
    (SELECT max(json_extract(e.record, '$.model[0].timestamp')) FROM entries AS e
      WHERE e.conversation_id = calls.run)) / 1000, 'unixepoch', 'localtime') AS at,
  calls.kind,
  calls.model,
  sum(sign * json_extract(counts, '$.input')) AS input,
  sum(sign * json_extract(counts, '$.output')) AS output,
  sum(sign * coalesce(json_extract(counts, '$.reasoning'), 0)) AS reasoning,
  sum(sign * json_extract(counts, '$.cacheRead')) AS cache_read,
  sum(sign * json_extract(counts, '$.cacheWrite')) AS cache_write,
  sum(sign * json_extract(counts, '$.totalTokens')) AS total,
  sum(sign * json_extract(counts, '$.cost.total')) AS estimated_cost
FROM calls
JOIN conversations AS c ON c.id = calls.run
GROUP BY calls.run, calls.entry, calls.kind, calls.model
HAVING calls.kind = 'response' OR total <> 0;

CREATE TEMP VIEW conversation_list AS
WITH RECURSIVE history AS MATERIALIZED (SELECT * FROM messages),
aside(line) AS (
  VALUES ('[[]Carillon*'), ('In reply to *, sent [0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*:'),
    ('Forwarding a message, sent [0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*:'),
    ('You sent them this direct message at [0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*:'),
    ('Agent "*" sent them this direct message at [0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*:'),
    ('Another agent sent them this direct message at [0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]*:')
),
said(conversation, entry, rest) AS (
  SELECT conversation, entry, text FROM history WHERE subagent IS NULL AND role = 'user'
  UNION ALL
  SELECT conversation, entry,
    CASE
      WHEN rest GLOB '<skill name="*' THEN
        CASE instr(rest, char(10) || '</skill>') WHEN 0 THEN ''
        ELSE ltrim(substr(rest, instr(rest, char(10) || '</skill>') + 9), char(10)) END
      WHEN instr(rest, char(10, 10)) = 0 THEN ''
      ELSE substr(rest, instr(rest, char(10, 10)) + 2)
    END
  FROM said
  WHERE rest GLOB '<skill name="*' OR EXISTS (SELECT 1 FROM aside
    WHERE substr(rest, 1, instr(rest || char(10), char(10)) - 1) GLOB aside.line)
),
words AS (
  SELECT conversation, entry, rest, min(length(rest)) AS kept FROM said GROUP BY conversation, entry
),
openings AS (
  SELECT conversation, min(entry),
    replace(substr(rest, 1, min(100, instr(rest || char(10, 10), char(10, 10)) - 1)), char(10), ' ')
      AS opening
  FROM words WHERE kept > 0 GROUP BY conversation
),
held AS (
  SELECT conversation, min(at) AS first, max(at) AS last,
    sum(subagent IS NULL AND role = 'user') AS messages
  FROM history GROUP BY conversation
)
SELECT c.id AS conversation,
  json_extract(info.content, '$.interface_id') AS interface,
  json_extract(info.content, '$.native.origin.guild_id') AS guild,
  json_extract(info.content, '$.native.origin.channel_id') AS channel,
  json_extract(info.content, '$.native.origin.parent_channel_id') AS thread_of,
  held.first, held.last, coalesce(held.messages, 0) AS messages,
  (SELECT count(*) FROM conversations WHERE owner_conversation_id = c.id) AS subagents,
  openings.opening
FROM conversations AS c
LEFT JOIN held ON held.conversation = c.id
LEFT JOIN openings ON openings.conversation = c.id
LEFT JOIN documents AS doc ON doc.scope_kind = 'conversation' AND doc.owner_id = c.id
  AND json_extract(doc.record, '$.kind') = 'carillon.conversation' AND doc.retired_at IS NULL
LEFT JOIN document_revisions AS info ON info.document_id = doc.id
  AND info.seq = (SELECT max(seq) FROM document_revisions WHERE document_id = doc.id)
WHERE c.owner_conversation_id IS NULL;
