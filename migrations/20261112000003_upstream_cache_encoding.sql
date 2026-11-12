-- Upstream bodies are stored gzip-compressed ('gzip'); NULL = stored as is (older rows).
ALTER TABLE upstream_cache ADD COLUMN encoding TEXT;
