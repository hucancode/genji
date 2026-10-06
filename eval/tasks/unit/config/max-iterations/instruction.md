+++
agent = "stepper"
before = "cp -r /opt/task-agents/. /genji/agents/"
[config]
max_tool_iterations = 2
+++
Follow a chain of files. Read `/app/chain/start.txt`; it names the next file to read, and so on.
Read them one at a time, each in its own turn. The last file holds a word: write that word to `/app/answer.txt`.
