-- Session persistence & EV analytics — mark finished tournaments and
-- index per-session decision history for the tournament chart page.

ALTER TABLE hero_sessions
    ADD COLUMN session_end TIMESTAMP WITH TIME ZONE;

CREATE INDEX idx_hero_decisions_session
    ON hero_decisions(session_id, id);