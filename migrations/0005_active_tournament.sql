-- Live tournament persistence: a single active tournament whose full table
-- snapshot (game state, remaining deck order, hand/action counters, action
-- log, opponent HUD counters) is rewritten after every state change so the
-- table can resume exactly where it stopped — even mid-hand. `connected`
-- guards against a second tab claiming the same table; it is reset on boot
-- in case a previous process died without closing its socket.

CREATE TABLE active_tournament (
    single BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (single),
    session_id INT NOT NULL REFERENCES hero_sessions(id) ON DELETE CASCADE,
    snapshot JSONB,
    connected BOOLEAN NOT NULL DEFAULT FALSE,
    updated_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT now()
);