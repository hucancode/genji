#!/bin/bash
cat > /app/textstats.py <<'PY'
import json
import string
import sys
from collections import Counter


def stats(text):
    words = text.split()
    norm = [w.strip(string.punctuation).lower() for w in words]
    norm = [w for w in norm if w]
    counts = Counter(norm)
    top = max(norm, key=lambda w: (counts[w], -norm.index(w))) if norm else None
    return {"lines": len(text.splitlines()), "words": len(words), "chars": len(text), "top_word": top}


if __name__ == "__main__":
    with open(sys.argv[1]) as f:
        print(json.dumps(stats(f.read())))
PY
cat > /app/test_textstats.py <<'PY'
import unittest

from textstats import stats


class StatsTest(unittest.TestCase):
    def test_counts(self):
        self.assertEqual(stats("a b\nb\n"), {"lines": 2, "words": 3, "chars": 6, "top_word": "b"})


if __name__ == "__main__":
    unittest.main()
PY
