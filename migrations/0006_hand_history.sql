-- GGPoker hand-history import: imported tournaments (one row per played
-- tournament, filled in by the hand files and the tournament-summary files
-- exported by PokerCraft) and imported hands (one row per parsed hand).
-- Both tables use the natural PokerCraft identifiers as primary keys, so a
-- re-scan of the same zips is idempotent. Timestamps are kept as PokerCraft's
-- own "YYYY-MM-DD HH:MM:SS" text (it sorts correctly and avoids time-zone
-- ambiguity in the export files); money is stored in cents.

CREATE TABLE gg_tournaments (
    id TEXT PRIMARY KEY,             -- PokerCraft tournament number, e.g. "307865587"
    name TEXT NOT NULL,              -- e.g. "Spin&Gold #7"
    game_type TEXT,                  -- e.g. "Hold'em No Limit"
    started_at TEXT NOT NULL,        -- first hand / summary stated start
    finished_at TEXT,                -- last hand timestamp
    buy_in_cents INT,                -- from the tournament summary file
    prize_cents INT,                 -- from the tournament summary file
    place INT,                       -- hero's finish position; 1 = won
    entrants INT,
    created_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT now(),
    updated_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT now()
);

CREATE TABLE gg_hands (
    hand_id TEXT PRIMARY KEY,        -- e.g. "SG4176965290"
    tournament_id TEXT NOT NULL REFERENCES gg_tournaments(id) ON DELETE CASCADE,
    played_at TEXT NOT NULL,
    sb INT NOT NULL,
    bb INT NOT NULL,
    position TEXT NOT NULL,          -- BTN / SB / BB
    table_size INT NOT NULL,
    hero_stack INT,                  -- hero's chips at the start of the hand
    hero_cards TEXT,
    all_in BOOLEAN NOT NULL,
    showdown BOOLEAN NOT NULL,
    hero_won BOOLEAN NOT NULL,
    invested INT NOT NULL,           -- chips the hero put into the pot
    collected INT NOT NULL,          -- chips the hero collected back
    net INT NOT NULL,                -- collected - invested
    board TEXT,
    raw TEXT NOT NULL                -- the hand block as written by PokerCraft
);

CREATE INDEX idx_gg_hands_tournament
    ON gg_hands(tournament_id, played_at DESC);