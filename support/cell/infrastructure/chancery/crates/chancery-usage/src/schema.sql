CREATE TABLE systems (id TEXT PRIMARY KEY NOT NULL) STRICT;
CREATE TABLE commands (
    system_id TEXT NOT NULL REFERENCES systems(id),
    id TEXT NOT NULL,
    PRIMARY KEY (system_id, id)
) STRICT;
CREATE TABLE usage (
    id INTEGER PRIMARY KEY,
    recorded_at INTEGER NOT NULL,
    codex_thread_id TEXT,
    system_id TEXT NOT NULL,
    command_id TEXT NOT NULL,
    FOREIGN KEY (system_id, command_id) REFERENCES commands(system_id, id)
) STRICT;
CREATE INDEX usage_command_time ON usage(system_id, command_id, recorded_at);
CREATE INDEX usage_thread ON usage(codex_thread_id, id);
CREATE TRIGGER usage_no_update BEFORE UPDATE ON usage BEGIN SELECT RAISE(ABORT, 'usage is append-only'); END;
CREATE TRIGGER usage_no_delete BEFORE DELETE ON usage BEGIN SELECT RAISE(ABORT, 'usage is append-only'); END;
CREATE TRIGGER systems_no_update BEFORE UPDATE ON systems BEGIN SELECT RAISE(ABORT, 'system identities are immutable'); END;
CREATE TRIGGER systems_no_delete BEFORE DELETE ON systems BEGIN SELECT RAISE(ABORT, 'system identities are retained'); END;
CREATE TRIGGER commands_no_update BEFORE UPDATE ON commands BEGIN SELECT RAISE(ABORT, 'command identities are immutable'); END;
CREATE TRIGGER commands_no_delete BEFORE DELETE ON commands BEGIN SELECT RAISE(ABORT, 'command identities are retained'); END;
