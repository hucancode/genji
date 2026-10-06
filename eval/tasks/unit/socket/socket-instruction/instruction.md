+++
socket = true
socket_send = [
  { after_tool_calls = 1, line = "/ping" },
  { after_tool_calls = 1, line = "Also create /app/b.txt containing b" },
]
+++
Do these steps one at a time, one tool call per step:

1. Run `ls /app` with the bash tool.
2. Read `/app/notes.txt`.
3. Write the word that `/app/notes.txt` names to `/app/a.txt`, with nothing else in the file.
