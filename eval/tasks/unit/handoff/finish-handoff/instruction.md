+++
agent = "dispatcher"
follow_handoffs = 1
before = "cp -r /opt/task-agents/. /genji/agents/"
+++
Do not do any work yourself. Call `finish` with status `handoff`, `next.agent` = `build` and `next.task` = `Create /app/result.txt containing handed`.
