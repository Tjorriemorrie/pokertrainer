-- The hero's own locally-generated decisions (from both /play and /drill
-- sessions), where the engine's true dealt hole cards are always known.
-- Fills out the hero's "latest 1000 actions" starting-hand window whenever
-- the imported gg_hands alone don't cover it — mirrors
-- local_opponent_actions (0009_opponent_history.sql), see
-- src/opponent_history.rs.
CREATE TABLE local_hero_actions (
    id BIGSERIAL PRIMARY KEY,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    node TEXT NOT NULL,
    stack_bucket SMALLINT NOT NULL,
    hole_cards TEXT NOT NULL,       -- e.g. "As Kh"
    action TEXT NOT NULL            -- "Fold" / "CallCheck" / "BetRaise"
);

CREATE INDEX idx_local_hero_actions_created
    ON local_hero_actions(created_at DESC);
