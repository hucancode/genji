FROM python:3.12-slim
RUN apt-get update && apt-get install -y --no-install-recommends git ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY wrap.py /opt/task/wrap.py
COPY notes.txt /app/notes.txt
RUN git init -q && git add -A && git -c user.name=eval -c user.email=eval@local commit -q --allow-empty -m baseline
