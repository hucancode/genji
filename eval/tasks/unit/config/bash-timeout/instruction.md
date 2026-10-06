+++
[config]
bash_timeout_secs = 2
+++
Run the command `sleep 30; echo hi` once with the bash tool. Do not set `timeout_secs`, and do not run it again whatever happens.
Then write `ok` to `/app/answer.txt`, with nothing else in the file.
