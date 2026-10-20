# Real fixture notes
Captured via local capture server (127.0.0.1:9999, DSN key abcdef0123456789abcdef0123456789, project 1). Bodies unaltered.
- node_*: @sentry/node 11.8.0 (Node 26). Node SDK sends auth in URL query (no x-sentry-auth header, no gzip). node_session_init/end are `session` envelopes; the others are `event`. No dsn field in envelope headers.
- ruby_*: sentry-ruby 5.28.1 (system Ruby 2.6.10 cannot run newer; 5.28.1 is the last supporting it). Uses x-sentry-auth header, `event` items only. ruby_cause: raise in rescue (implicit cause).
- browser_exception: @sentry/browser 11.8.0 run in node with window/document shims (no integrations, fetch transport). REAL envelope shape, but stack frames are Node-style filenames, not browser URLs.
- nextjs_shape.envelope: HAND-MADE (not captured) approximating @sentry/nextjs v9 server output; has debug_meta sourcemap image and in_app false node_modules frames.
- node_exception.envelope.gz: HAND-GZIPPED (gzip -9) copy of node_exception.envelope; no SDK sent compressed bodies (node/ruby/browser all uncompressed, small).
- ruby_* headers.json have empty content-encoding (none).
