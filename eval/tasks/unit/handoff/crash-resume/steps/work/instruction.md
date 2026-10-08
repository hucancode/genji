+++
wrap = "python3 /opt/task/wrap.py"
+++
Write `/app/textstats.py`, a command-line tool: `python3 textstats.py FILE` prints one JSON object with

- `lines`: number of lines,
- `words`: number of whitespace-separated words,
- `chars`: number of characters,
- `top_word`: the most frequent word, lowercased with surrounding punctuation stripped (ties go to the word that appears first).

Add unit tests in `/app/test_textstats.py` (stdlib `unittest`) and make sure they pass.
