-- S1: base schema for the local pokertrainer database.

-- Base opponent identities and broad player types (e.g. LAG, NIT).
CREATE TABLE opponent_profiles (
    id SERIAL PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    player_type VARCHAR(30) NOT NULL,
    created_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP
);

-- VPIP/PFR/3-Bet/C-Bet, split by Spin & Go stack-depth buckets (25/15/10 BB).
CREATE TABLE opponent_stats (
    id SERIAL PRIMARY KEY,
    profile_id INT NOT NULL REFERENCES opponent_profiles(id) ON DELETE CASCADE,
    stack_bucket SMALLINT NOT NULL CHECK (stack_bucket IN (10, 15, 25)),
    vpip REAL NOT NULL DEFAULT 0,
    pfr REAL NOT NULL DEFAULT 0,
    three_bet REAL NOT NULL DEFAULT 0,
    c_bet REAL NOT NULL DEFAULT 0,
    hands INT NOT NULL DEFAULT 0,
    UNIQUE (profile_id, stack_bucket)
);

-- Sequence node (e.g. BTN_OPEN_2BB_SB_FOLD) -> per-profile 169-hand range
-- matrix (AA..72o, row-major 13x13), sampled in-memory by the MCTS engine.
CREATE TABLE contextual_ranges (
    id SERIAL PRIMARY KEY,
    node TEXT NOT NULL,
    profile_id INT NOT NULL REFERENCES opponent_profiles(id) ON DELETE CASCADE,
    weights REAL[169] NOT NULL,
    updated_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (node, profile_id)
);

-- Session metadata; per-decision EV records live in hero_decisions.
CREATE TABLE hero_sessions (
    id SERIAL PRIMARY KEY,
    session_start TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    opponent_1_archetype VARCHAR(30),
    opponent_2_archetype VARCHAR(30)
);

CREATE TABLE hero_decisions (
    id SERIAL PRIMARY KEY,
    session_id INT REFERENCES hero_sessions(id) ON DELETE CASCADE,
    hand_number INT NOT NULL,
    street INT NOT NULL, -- 0: Preflop, 1: Flop, 2: Turn, 3: River
    played_action VARCHAR(20) NOT NULL,
    optimal_action VARCHAR(20) NOT NULL,
    ev_loss FLOAT NOT NULL, -- Calculated by Expectimax (0.0 if optimal)
    created_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX idx_recent_decisions ON hero_decisions(created_at DESC);