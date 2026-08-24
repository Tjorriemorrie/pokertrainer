-- Tournament results: per-hand outcomes and the final result of a finished
-- tournament, feeding the tournament detail page.

CREATE TABLE hero_hand_results (
    id SERIAL PRIMARY KEY,
    session_id INT NOT NULL REFERENCES hero_sessions(id) ON DELETE CASCADE,
    hand_number INT NOT NULL,
    hero_won BOOLEAN NOT NULL,
    hero_all_in BOOLEAN NOT NULL,
    hero_busted BOOLEAN NOT NULL,
    winner_seat INT NOT NULL, -- 0: Hero, 1: Opponent 1, 2: Opponent 2
    created_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX idx_hero_hand_results_session
    ON hero_hand_results(session_id, hand_number);

ALTER TABLE hero_sessions
    ADD COLUMN result VARCHAR(10),
    ADD COLUMN final_stack INT;
