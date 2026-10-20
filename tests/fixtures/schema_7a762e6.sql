-- OpenFault schema v1. ponytail: no migration framework; v2 adds `ALTER`s keyed on schema_version.
-- Timestamps are unix seconds (INTEGER).
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);

CREATE TABLE IF NOT EXISTS orgs (
  id INTEGER PRIMARY KEY, name TEXT NOT NULL, slug TEXT NOT NULL UNIQUE,
  settings_json TEXT NOT NULL DEFAULT '{}', created_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS users (
  id INTEGER PRIMARY KEY, email TEXT NOT NULL UNIQUE, name TEXT NOT NULL DEFAULT '',
  pw_hash TEXT NOT NULL, is_owner INTEGER NOT NULL DEFAULT 0, role TEXT NOT NULL DEFAULT 'member', -- owner|admin|member
  created_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS sessions (
  id_hash TEXT PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  expires_at INTEGER NOT NULL, ua TEXT);
CREATE TABLE IF NOT EXISTS invites (
  token_hash TEXT PRIMARY KEY, email TEXT NOT NULL, role TEXT NOT NULL, project_ids_json TEXT NOT NULL DEFAULT '[]',
  invited_by INTEGER REFERENCES users(id), expires_at INTEGER NOT NULL, accepted_at INTEGER);
CREATE TABLE IF NOT EXISTS password_resets (
  token_hash TEXT PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE, expires_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS projects (
  id INTEGER PRIMARY KEY, org_id INTEGER NOT NULL REFERENCES orgs(id), slug TEXT NOT NULL UNIQUE, name TEXT NOT NULL,
  platform TEXT, settings_json TEXT NOT NULL DEFAULT '{}', retention_days INTEGER NOT NULL DEFAULT 90,
  created_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS project_members (
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE, role TEXT NOT NULL,
  PRIMARY KEY (project_id, user_id));
CREATE TABLE IF NOT EXISTS dsn_keys (
  id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  public_key TEXT NOT NULL UNIQUE, label TEXT NOT NULL DEFAULT 'Default', active INTEGER NOT NULL DEFAULT 1,
  created_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS api_tokens (
  id INTEGER PRIMARY KEY, token_hash TEXT NOT NULL UNIQUE, user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  project_id INTEGER REFERENCES projects(id) ON DELETE CASCADE, scopes TEXT NOT NULL, label TEXT, last_used INTEGER);
CREATE TABLE IF NOT EXISTS issues (
  id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  fingerprint TEXT NOT NULL, title TEXT NOT NULL, culprit TEXT NOT NULL DEFAULT '', level TEXT NOT NULL DEFAULT 'error',
  platform TEXT, status TEXT NOT NULL DEFAULT 'unresolved', resolved_in_release TEXT,
  resolved_next_release INTEGER NOT NULL DEFAULT 0, mute_until INTEGER, mute_after_count INTEGER, mute_window_min INTEGER,
  first_seen INTEGER NOT NULL, last_seen INTEGER NOT NULL, event_count INTEGER NOT NULL DEFAULT 0,
  user_count INTEGER NOT NULL DEFAULT 0, stored_count INTEGER NOT NULL DEFAULT 0,
  first_release TEXT, last_release TEXT, fixed_at_releases_json TEXT, approved_by INTEGER, approved_at INTEGER,
  fix_link TEXT, assignee_id INTEGER, last_spike_at INTEGER);
CREATE INDEX IF NOT EXISTS issues_project_status ON issues(project_id, status, last_seen);
-- deviation: project_id added so identical bugs in two projects never merge.
CREATE TABLE IF NOT EXISTS issue_hashes (
  project_id INTEGER NOT NULL, hash TEXT NOT NULL, issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
  PRIMARY KEY (project_id, hash));
CREATE TABLE IF NOT EXISTS events (
  id TEXT PRIMARY KEY, issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
  project_id INTEGER NOT NULL, received_at INTEGER NOT NULL, timestamp INTEGER, release TEXT, environment TEXT,
  user_hash TEXT, sampled INTEGER NOT NULL DEFAULT 0, payload BLOB NOT NULL);
CREATE INDEX IF NOT EXISTS events_project_received ON events(project_id, received_at);
CREATE INDEX IF NOT EXISTS events_issue_received ON events(issue_id, received_at);
-- every processed event_id (stored or sampled out), for dedupe of retries; purged after 7 days.
CREATE TABLE IF NOT EXISTS seen_events (id TEXT PRIMARY KEY, received_at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS seen_events_received ON seen_events(received_at);
CREATE TABLE IF NOT EXISTS issue_hourly (
  issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE, hour_ts INTEGER NOT NULL,
  count INTEGER NOT NULL DEFAULT 0, user_count_est INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (issue_id, hour_ts));
CREATE TABLE IF NOT EXISTS project_hourly (
  project_id INTEGER NOT NULL, hour_ts INTEGER NOT NULL, received INTEGER NOT NULL DEFAULT 0,
  stored INTEGER NOT NULL DEFAULT 0, dropped INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (project_id, hour_ts));
-- received events per 5-minute bucket for /live/summary; purged after 24h.
CREATE TABLE IF NOT EXISTS project_5min (
  project_id INTEGER NOT NULL, bucket_ts INTEGER NOT NULL, count INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (project_id, bucket_ts));
CREATE TABLE IF NOT EXISTS issue_users (
  issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE, user_hash TEXT NOT NULL, last_seen INTEGER NOT NULL,
  PRIMARY KEY (issue_id, user_hash));
CREATE TABLE IF NOT EXISTS releases (
  project_id INTEGER NOT NULL, version TEXT NOT NULL, is_semver INTEGER NOT NULL, sort_key TEXT NOT NULL,
  first_seen INTEGER NOT NULL, commit_sha TEXT, url TEXT, PRIMARY KEY (project_id, version));
CREATE TABLE IF NOT EXISTS issue_activity (
  id INTEGER PRIMARY KEY, issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE, user_id INTEGER,
  kind TEXT NOT NULL, data_json TEXT, created_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS artifact_bundles (
  id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE, debug_id TEXT NOT NULL,
  minified_cache BLOB, map_cache BLOB, created_at INTEGER NOT NULL);
CREATE UNIQUE INDEX IF NOT EXISTS artifact_bundles_debug ON artifact_bundles(project_id, debug_id);
-- sentry-cli chunk-upload staging; rows are deleted once assembled.
CREATE TABLE IF NOT EXISTS chunks (sha1 TEXT PRIMARY KEY, data BLOB NOT NULL, created_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS devices (
  id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE, platform TEXT NOT NULL,
  push_token TEXT, created_at INTEGER NOT NULL, name TEXT, token_id INTEGER);
-- NULL push_tokens are distinct, so a user may have several devices awaiting APNs.
CREATE UNIQUE INDEX IF NOT EXISTS devices_user_token ON devices(user_id, push_token);
-- QR pairing (src/pairing.rs): one-time codes, stored hashed; token_id = the api_tokens row the claim minted.
CREATE TABLE IF NOT EXISTS pairings (
  code_hash TEXT PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL, claimed_at INTEGER, token_id INTEGER);
CREATE TABLE IF NOT EXISTS notif_prefs (
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE, project_id INTEGER, kind TEXT NOT NULL,
  email INTEGER NOT NULL DEFAULT 1, push INTEGER NOT NULL DEFAULT 1);
CREATE TABLE IF NOT EXISTS webhooks (
  id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE, kind TEXT NOT NULL,
  url TEXT NOT NULL, enabled INTEGER NOT NULL DEFAULT 1, last_error TEXT, secret TEXT, last_ok_at INTEGER);
-- notification pipeline (src/notify): one row per trigger; kept 24h as the Settings delivery log.
CREATE TABLE IF NOT EXISTS notif_queue (
  id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, issue_id INTEGER NOT NULL, kind TEXT NOT NULL,
  payload_json TEXT NOT NULL, created_at INTEGER NOT NULL, attempts INTEGER NOT NULL DEFAULT 0,
  last_error TEXT, done_at INTEGER);
CREATE INDEX IF NOT EXISTS notif_queue_pending ON notif_queue(done_at, attempts);
-- per channel deliveries; user_id 0 + channel 'webhook:{id}' for webhooks. Dedupe window 1h, purged after 24h.
CREATE TABLE IF NOT EXISTS notif_sent (
  user_id INTEGER NOT NULL, issue_id INTEGER NOT NULL, kind TEXT NOT NULL, channel TEXT NOT NULL, sent_at INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS notif_sent_key ON notif_sent(issue_id, kind, user_id, channel, sent_at);
CREATE TABLE IF NOT EXISTS inbox (
  id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, received_at INTEGER NOT NULL, body BLOB NOT NULL,
  content_encoding TEXT NOT NULL DEFAULT 'zstd');

CREATE TABLE IF NOT EXISTS saved_views (
  id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE, name TEXT NOT NULL,
  query TEXT NOT NULL, sort TEXT NOT NULL DEFAULT 'last_seen', created_at INTEGER NOT NULL);

INSERT INTO schema_version (version) SELECT 1 WHERE NOT EXISTS (SELECT 1 FROM schema_version);
