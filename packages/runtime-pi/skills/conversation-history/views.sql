CREATE TEMP VIEW messages AS
SELECT * FROM (
  SELECT e.conversation_id AS conversation,
    e.id AS entry,
    datetime(json_extract(e.record, '$.model[0].timestamp') / 1000, 'unixepoch', 'localtime') AS at,
    json_extract(e.record, '$.model[0].role') AS role,
    CASE json_type(e.record, '$.model[0].content')
      WHEN 'text' THEN json_extract(e.record, '$.model[0].content')
      ELSE (SELECT group_concat(json_extract(part.value, '$.text'), char(10))
        FROM json_each(e.record, '$.model[0].content') AS part
        WHERE json_extract(part.value, '$.type') = 'text')
    END AS text
  FROM entries AS e
  WHERE json_extract(e.record, '$.kind') IN ('carillon.message', 'pi.assistant')
)
WHERE text <> '';

CREATE TEMP VIEW usage AS
SELECT e.conversation_id AS conversation,
  datetime(json_extract(e.record, '$.model[0].timestamp') / 1000, 'unixepoch', 'localtime') AS at,
  json_extract(e.record, '$.model[0].provider') || '/' || json_extract(e.record, '$.model[0].model') AS model,
  json_extract(e.record, '$.model[0].usage.input') AS input,
  json_extract(e.record, '$.model[0].usage.output') AS output,
  json_extract(e.record, '$.model[0].usage.reasoning') AS reasoning,
  json_extract(e.record, '$.model[0].usage.cacheRead') AS cache_read,
  json_extract(e.record, '$.model[0].usage.cacheWrite') AS cache_write,
  json_extract(e.record, '$.model[0].usage.totalTokens') AS total,
  json_extract(e.record, '$.model[0].usage.cost.total') AS estimated_cost
FROM entries AS e
WHERE json_extract(e.record, '$.kind') = 'pi.assistant';

CREATE TEMP VIEW conversation_list AS
SELECT c.id AS conversation,
  c.owner_conversation_id AS started_by,
  json_extract(info.content, '$.interface_id') AS interface,
  json_extract(info.content, '$.native.origin.guild_id') AS guild,
  json_extract(info.content, '$.native.origin.channel_id') AS channel,
  json_extract(info.content, '$.native.origin.parent_channel_id') AS thread_of,
  (SELECT min(at) FROM messages WHERE conversation = c.id) AS first,
  (SELECT max(at) FROM messages WHERE conversation = c.id) AS last,
  (SELECT count(*) FROM messages WHERE conversation = c.id AND role = 'user') AS messages
FROM conversations AS c
LEFT JOIN documents AS doc ON doc.scope_kind = 'conversation' AND doc.owner_id = c.id
  AND json_extract(doc.record, '$.kind') = 'carillon.conversation' AND doc.retired_at IS NULL
LEFT JOIN document_revisions AS info ON info.document_id = doc.id
  AND info.seq = (SELECT max(seq) FROM document_revisions WHERE document_id = doc.id);
