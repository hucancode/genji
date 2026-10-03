---
description: Track work as requirements and tickets in .genji/ markdown files (plan side and build side workflows)
---
Requirements are the success criteria; tickets are the work queue. Both are markdown files with frontmatter. Create and edit them with `write`/`edit`, read them with `read`/`ls`/`bash`.

## Files
`.genji/requirements/<id>.md`
```
---
id: 3
level: stakeholder        # stakeholder | system
status: active            # active | met
parent_id: 1              # system requirements point at a stakeholder requirement
---
Text of the requirement. Open questions go under an "Open questions" heading with options and a recommended default.
```
`.genji/tickets/<id>.md`
```
---
id: 7
title: Short title
status: open              # open | in_progress | resolved | closed
priority: 2               # 1 (high) to 3 (low)
requirement_id: 3
parent_id:                # optional, for sub-tickets
---
What to do, how to verify it. Record assumptions here.
```
New ids are max existing id + 1 within the directory.

## Claiming a ticket
`mkdir .genji/claims/<id>` is atomic: success means the ticket is yours, failure means another agent holds it. Then set `status: in_progress`. Never work on a ticket you did not claim.

## Plan side
1. Read the requirements and the tickets; explore the workspace enough to understand the current state.
2. Derive concrete system requirements from stakeholder requirements.
3. Create tickets for concrete units of work, each linked with `requirement_id`.
4. Raise each ambiguous requirement as an open question in its file; do not guess silently.
5. Check coverage: every active requirement has a path to being met.
6. Write or update the plan with `plan_write` so it matches the requirements and tickets.
7. Mark a requirement `met` only when you are confident the current artifacts satisfy it. Goal done = no active requirement is left unmet.

## Build side
1. Read the requirements and open tickets; claim the highest-value open ticket.
2. Do the work, then verify it (build, test, run).
3. Set the ticket `status: resolved` once done and verified (`closed` for obsolete or duplicate).
4. Repeat until no actionable ticket remains, then report.
Do not create requirements here. When a ticket is ambiguous, take the most reasonable reading and record the assumption in the ticket and your report. Report missing work you discover.
