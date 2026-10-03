# Tickets and requirements

The requirements/tickets system is compiled only with the `formal` Cargo
feature. Build that variant with `cargo build --features formal`, then enable it
at runtime with `genji --formal ...` (for example `genji plan --formal "..."`).
A binary built without the feature has no `--formal` flag, does not create or
read requirement/ticket files, and does not offer those tools to the model. See
[Modes](modes.md#formal-requirements--tickets).

Requirements have two levels:

- **stakeholder** — intent, as written by the user.
- **system** — derived, verifiable statements that satisfy stakeholder intent.

Lifecycle: `active` → `met` (satisfied) or `removed`. The automatic cycle stops
when it can no longer find any `active` requirement. Build mode deliberately
cannot create/update requirements; it closes tickets and reports. Plan mode
owns requirement status, and also persists its human-readable plan under
`.genji/plans/` (see [Modes](modes.md#plans)).

Requirements are persisted as markdown files directly under
`.genji/requirements/` (`<id>-<slug>.md`). Nothing is stored in SQLite: tickets
reference a requirement by the numeric `id` recorded in the file's frontmatter,
and the level (`stakeholder`/`system`) lives in the frontmatter as well.

Tickets are also Markdown-only under `.genji/tickets/` (`<id>-<slug>.md`).
Status and resolution remain in frontmatter, including for closed tickets.
There is one source of truth and no migration or archive database.

```
requirement_create  {level, title, body, parent_id?}
requirement_read    {id?, level?, status?}   # by id: one requirement; no id: list/filter
requirement_tree    {status?}                     # hierarchy + ticket coverage
requirement_update  {id, title?, body?, status?, level?, parent_id?}
requirement_remove  {id, hard?}
requirement_ask     {question, requirement_id?}   # plan only; recorded in DB for a human to resolve

ticket_create       {title, description?, priority?, parent_id?, requirement_id?}
ticket_read         {id?, status?, requirement_id?}  # by id: any ticket; no id: actionable files only
ticket_claim        {id?, requirement_id?}        # claim next ticket, mark in_progress
ticket_update       {id, title?, description?, priority?, parent_id?, requirement_id?, status?, resolution?}
ticket_close        {id, reason?}
```

---

## Requirement files (markdown)

Every requirement is a markdown file under `.genji/requirements/`. The agent
creates and edits them through the `requirement_*` tools; you can also drop
your own files in (they are loaded on every startup when
`auto_ingest_requirements` is true).

- The **level** comes from the frontmatter `level:`, defaulting to
  `stakeholder` when absent.
- The **title** is the frontmatter `title:` or the first `# Heading`, falling
  back to the file name.
- The **body** is everything after the first `# Heading`.
- The **id** is the number in frontmatter; tickets reference it. Files without
  one are assigned the next free id on load. Editing the title renames the
  file to a matching `<id>-<slug>.md`.

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

---

## Ticket files (markdown)

Every open ticket is a markdown file under `.genji/tickets/`. As with
requirements, you can drop your own files in and they are picked up.

- The **status** comes from the frontmatter `status:`, defaulting to `open`.
  Only `open` and `in_progress` belong here; terminal tickets live in the DB.
- The **title** is the frontmatter `title:` or the first `# Heading`, falling
  back to the file name.
- The **description** is everything after the first `# Heading`.
- The **id** is the number in frontmatter; requirements and parent tickets
  reference it. Files without one are assigned the next free id on load.
  Editing the title renames the file to a matching `<id>-<slug>.md`.
- `priority` (1 high – 3 low), `parent`, `requirement` and `mode` are optional.

```markdown
---
id: 7
status: in_progress
priority: 1
requirement: 4
mode: build
created: 1700000000
updated: 1700000000
---
# Add rate limiting

Reject more than 100 requests/minute per key with HTTP 429 and cover it with a test.
```
