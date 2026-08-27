-- Action-frequency model: per (node, profile, stack bucket, position,
-- aggressor context) fold/call-check/raise/shove mix, mirroring
-- contextual_ranges but tallying action categories instead of hand classes.
-- Feeds the bots' action-category selection (opponent_history's
-- ActionFrequencyModel) so they play the field's real fold/call/raise/shove
-- mix in a spot instead of always taking whatever the MCTS solve says is
-- best-EV regardless of realism.
CREATE TABLE contextual_action_frequencies (
    id SERIAL PRIMARY KEY,
    node TEXT NOT NULL,
    profile_id INT NOT NULL REFERENCES opponent_profiles(id) ON DELETE CASCADE,
    stack_bucket SMALLINT NOT NULL CHECK (stack_bucket IN (10, 15, 25)),
    position TEXT NOT NULL,
    aggressor_ctx TEXT NOT NULL,
    fold_pct REAL NOT NULL,
    call_check_pct REAL NOT NULL,
    raise_pct REAL NOT NULL,
    shove_pct REAL NOT NULL,
    sample_count INT NOT NULL DEFAULT 0,
    updated_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (node, profile_id, stack_bucket, position, aggressor_ctx)
);
