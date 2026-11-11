-- Instance settings that must not change once data exists (e.g. the UUID namespace).
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
