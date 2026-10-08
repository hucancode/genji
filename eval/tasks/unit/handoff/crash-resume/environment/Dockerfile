FROM python:3.12-slim
RUN apt-get update && apt-get install -y --no-install-recommends git jq netcat-openbsd ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY wrap.py /opt/task/wrap.py
RUN git init -q && git add -A && git -c user.name=eval -c user.email=eval@local commit -q --allow-empty -m baseline
