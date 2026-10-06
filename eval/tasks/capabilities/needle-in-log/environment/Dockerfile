FROM python:3.12-slim
RUN apt-get update && apt-get install -y --no-install-recommends git jq ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY generate_log.py /tmp/generate_log.py
RUN mkdir -p logs && python3 /tmp/generate_log.py && rm /tmp/generate_log.py \
    && git init -q && git add -A && git -c user.name=eval -c user.email=eval@local commit -qm baseline
