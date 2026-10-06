"""Writes /app/logs/service.log: ~50 MB of service log with exactly one ERROR line.

Thousands of WARN lines mention "error" in their message, so a case-insensitive grep
for "error" floods the agent's tool output.
"""

import random

NEEDLE = "a3f9c2e1"
random.seed(7)
paths = ["/api/orders", "/api/users", "/api/cart", "/health", "/api/search"]
lines = []
total = 0
i = 0
needle_at = 310_000
while total < 50_000_000:
    rid = f"{random.getrandbits(32):08x}"
    ts = f"2026-10-01T{(i // 3600) % 24:02d}:{(i // 60) % 60:02d}:{i % 60:02d}.{i % 1000:03d}Z"
    if i == needle_at:
        line = f"{ts} ERROR req={NEEDLE} path=/api/orders msg=\"payment provider rejected charge\""
    elif i % 37 == 0:
        line = f"{ts} WARN req={rid} path={random.choice(paths)} msg=\"retrying after upstream error=timeout attempt={i % 4}\""
    else:
        line = f"{ts} INFO req={rid} path={random.choice(paths)} status=200 ms={random.randint(1, 900)}"
    lines.append(line)
    total += len(line) + 1
    i += 1
with open("/app/logs/service.log", "w") as f:
    f.write("\n".join(lines) + "\n")
