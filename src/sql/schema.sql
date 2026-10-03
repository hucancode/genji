CREATE TABLE IF NOT EXISTS requirement_questions (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  requirement_id INTEGER,
  instance_id TEXT,
  question TEXT NOT NULL,
  answer TEXT,
  status TEXT NOT NULL DEFAULT 'open',
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  answered_at TEXT
);

CREATE TABLE IF NOT EXISTS instances (
  id TEXT PRIMARY KEY,
  mode TEXT NOT NULL,
  parent_instance TEXT,
  task TEXT,
  model TEXT,
  depth INTEGER NOT NULL DEFAULT 0,
  status TEXT NOT NULL DEFAULT 'running',
  tokens_used INTEGER NOT NULL DEFAULT 0,
  started_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  ended_at TEXT,
  report TEXT,
  pid INTEGER
);

CREATE TABLE IF NOT EXISTS messages (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  instance_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  role TEXT NOT NULL,
  content TEXT NOT NULL DEFAULT '',
  tool_calls TEXT,
  tool_call_id TEXT,
  reasoning TEXT,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX IF NOT EXISTS idx_messages_instance ON messages(instance_id, seq);
DROP INDEX IF EXISTS idx_messages_role;

CREATE TABLE IF NOT EXISTS tool_calls (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  instance_id TEXT NOT NULL,
  message_seq INTEGER NOT NULL DEFAULT 0,
  call_id TEXT,
  name TEXT NOT NULL,
  args TEXT NOT NULL DEFAULT '',
  result TEXT NOT NULL DEFAULT '',
  is_error INTEGER NOT NULL DEFAULT 0,
  duration_ms INTEGER NOT NULL DEFAULT 0,
  -- started | done | interrupted
  status TEXT NOT NULL DEFAULT 'done',
  child_instance TEXT,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX IF NOT EXISTS idx_toolcalls_instance ON tool_calls(instance_id);
CREATE INDEX IF NOT EXISTS idx_toolcalls_name ON tool_calls(name);

CREATE TABLE IF NOT EXISTS compactions (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  instance_id TEXT,
  removed_messages INTEGER NOT NULL DEFAULT 0,
  before_tokens INTEGER NOT NULL DEFAULT 0,
  after_tokens INTEGER NOT NULL DEFAULT 0,
  summary TEXT NOT NULL DEFAULT '',
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

CREATE TABLE IF NOT EXISTS context_checkpoints (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  instance_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  messages TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX IF NOT EXISTS idx_checkpoints_instance ON context_checkpoints(instance_id, id);
