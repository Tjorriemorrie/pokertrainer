-- Extends the per-hand opponent-analysis cache with hero's own decisions in
-- the same imported hands, graded the same way (solved from hero's seat with
-- hero's real known cards). Kept on the same row as the opponent columns
-- since both are produced by the same per-hand walk-and-grade pass.

ALTER TABLE gg_hand_analysis
    ADD COLUMN hero_decisions INT NOT NULL DEFAULT 0,
    ADD COLUMN hero_ev_loss_bb_sum DOUBLE PRECISION NOT NULL DEFAULT 0;
