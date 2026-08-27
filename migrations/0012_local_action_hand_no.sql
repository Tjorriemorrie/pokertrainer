-- Lets the starting-hand grids report how many *hands* their window covers,
-- not how many individual decisions — local play previously had no hand
-- identity at all, so the count could only ever be a decision count. Existing
-- rows default to 0 (they collapse into one undercounted "hand" bucket; only
-- new rows carry a real value), which is an acceptable one-time undercount
-- since the window is dominated by fresh data going forward.
ALTER TABLE local_opponent_actions ADD COLUMN hand_no BIGINT NOT NULL DEFAULT 0;
ALTER TABLE local_hero_actions ADD COLUMN hand_no BIGINT NOT NULL DEFAULT 0;
