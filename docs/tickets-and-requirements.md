# Tickets and requirements

The requirements/tickets system is only active when **OCD** is enabled
(`genji --ocd ...`, for example `genji plan --ocd "..."`). Without OCD, genji
is a plain coding agent and none of the `requirement_*`/`ticket_*` tools are
offered to the model. See [Modes](modes.md#ocd-requirements--tickets).

Requirements have two levels:

- **stakeholder** — intent, as written by the user.
- **system** — derived, verifiable statements that satisfy stakeholder intent.

Lifecycle: `active` → `met` (satisfied) or `removed`. The automatic cycle stops
when it can no longer find any `active` requirement. Build mode deliberately
cannot create/update requirements; it resolves tickets and reports. Plan mode
owns requirement status, and also persists its human-readable plan under
`.genji/plans/` (see [Modes](modes.md#plans)).

Requirements are persisted as markdown files under `.genji/requirements/`
(`<id>-<slug>.md` inside a `stakeholder/` or `system/` subdirectory). Nothing is
stored in SQLite: tickets reference a requirement by the numeric `id` recorded
in the file's frontmatter. A one-time migration exports any pre-existing rows
from the legacy `requirements` table on startup.

```
requirement_create  {level, title, body, parent_id?}
requirement_read    {id?, level?, status?}
requirement_tree    {status?}                     # hierarchy + ticket coverage
requirement_update  {id, title?, body?, status?, level?, parent_id?}
requirement_remove  {id, hard?}
requirement_ask     {question, requirement_id?}   # recorded in DB; answered if TTY

ticket_create       {title, description?, priority?, parent_id?, requirement_id?}
ticket_read         {id?, status?, requirement_id?}
ticket_claim        {id?, requirement_id?}        # claim next ticket, mark in_progress
ticket_update       {id, title?, description?, priority?, parent_id?, requirement_id?, status?, resolution?}
ticket_resolve      {id, resolution?}
ticket_close        {id, reason?}
ticket_reopen       {id}
```

---

## Requirement files (markdown)

Every requirement is a markdown file under `.genji/requirements/`. The agent
creates and edits them through the `requirement_*` tools; you can also drop
your own files in (they are loaded on every startup when
`auto_ingest_requirements` is true).

- The **level** comes from the frontmatter `level:` or, failing that, from the
  path (`…/system/…` → `system`, otherwise `stakeholder`).
- The **title** is the frontmatter `title:` or the first `# Heading`, falling
  back to the file name.
- The **body** is everything after the first `# Heading`.
- The **id** is the number in frontmatter; tickets reference it. Files without
  one are assigned the next free id on load. Editing the title or level moves
  the file to a matching `<id>-<slug>.md` name/directory.

```markdown
---
id: 7
level: system
status: active
source: user_md
created: 1700000000
updated: 1700000000
---
# Rate limiting

The API must reject more than 100 requests/minute per key with HTTP 429.
```
