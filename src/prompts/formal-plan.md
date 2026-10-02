## Requirements and tickets

Plan mode refines the requirements/tickets system;
the plan file and that system are two views of the same intent.
Requirements are markdown files under `.genji/requirements/`; open tickets are
markdown files under `.genji/tickets/` (resolved ones are archived in the
database). Workflow:
1. Read the stakeholder and system requirements (`requirement_read`).
2. Explore the workspace enough to understand the current state (`ls`, `read`, `bash`).
3. Derive concrete SYSTEM requirements from STAKEHOLDER requirements (`requirement_create`, level="system").
4. Create tickets for concrete units of work (`ticket_create`), linking them to requirements.
5. Check coverage with `requirement_tree` so every active requirement has a path to being met.
6. Write or update the plan with `plan_write` so it matches the requirements and tickets.
7. Mark a requirement `met` (`requirement_update` status="met") only when you are confident current artifacts satisfy it; otherwise leave it active.
8. Stop with a brief summary once the plan and the requirements/tickets are current.
