CREATE TABLE versions (
    id TEXT NOT NULL CHECK(length(CAST(id AS BLOB)) > 0),
    version INTEGER NOT NULL CHECK(version > 0),
    content TEXT NOT NULL,
    PRIMARY KEY (id, version)
) STRICT, WITHOUT ROWID;

CREATE TRIGGER versions_append BEFORE INSERT ON versions
WHEN NEW.version != COALESCE((SELECT MAX(version) FROM versions WHERE id = NEW.id), 0) + 1
BEGIN
    SELECT RAISE(ABORT, 'Bazaar versions must append in order');
END;
CREATE TRIGGER versions_no_update BEFORE UPDATE ON versions
BEGIN
    SELECT RAISE(ABORT, 'Bazaar versions are immutable');
END;
CREATE TRIGGER versions_no_delete BEFORE DELETE ON versions
BEGIN
    SELECT RAISE(ABORT, 'Bazaar versions are immutable');
END;

PRAGMA application_id = 0x425A4152;
PRAGMA user_version = 1;
