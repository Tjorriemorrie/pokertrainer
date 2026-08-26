-- The trainer's own locally-generated bot decisions (from both /play and
-- /drill sessions), where the engine's true dealt hole cards are always
-- known. This fills out the "latest 1000 actions" window whenever the
-- imported gg_hands alone (hole cards known only at showdown) don't cover
-- it — see src/opponent_history.rs.
CREATE TABLE local_opponent_actions (
    id BIGSERIAL PRIMARY KEY,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    node TEXT NOT NULL,
    stack_bucket SMALLINT NOT NULL,
    hole_cards TEXT NOT NULL,       -- e.g. "As Kh"
    action TEXT NOT NULL            -- "Fold" / "CallCheck" / "BetRaise"
);

CREATE INDEX idx_local_opponent_actions_created
    ON local_opponent_actions(created_at DESC);
