# Tickets and requirements

Requirements have two levels:

- **stakeholder** — intent, as written by the user.
- **system** — derived, verifiable statements that satisfy stakeholder intent.

Lifecycle: `active` → `met` (satisfied) or `removed`. The automatic cycle stops
when it can no longer find any `active` requirement. Build mode deliberately
cannot create/update requirements; it resolves tickets and reports. Plan mode
owns requirement status.

```
requirement_create  {level, title, body, parent_id?}
requirement_read    {id?, level?, status?}
requirement_update  {id, title?, body?, status?, level?, parent_id?}
requirement_remove  {id, hard?}
requirement_ask     {question, requirement_id?}   # recorded in DB; answered if TTY

ticket_create       {title, description?, priority?, parent_id?, requirement_id?}
ticket_read         {id?, status?, requirement_id?}
ticket_resolve      {id, resolution?}
ticket_close        {id, reason?}
```

---

## User-authored requirements (markdown)

Drop markdown files under `requirements/`. They are re-ingested on every
startup (`auto_ingest_requirements`):

- The **level** is inferred from the path (`…/system/…` → `system`, otherwise
  `stakeholder`) or from frontmatter `level: system`.
- The **title** is the first `# Heading`, falling back to the file name.
- The **body** is the whole file.
- Files are keyed by path: editing a file updates the same requirement. If the
  body changes, the requirement is reactivated for re-evaluation.

```markdown
---
level: system
---
# Rate limiting
The API must reject more than 100 requests/minute per key with HTTP 429.
```
