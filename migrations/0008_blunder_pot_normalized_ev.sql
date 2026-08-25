-- The coach's blunder-intervention threshold now compares EV losses as a
-- fraction of the pot at the decision point, not raw big blinds, so a river
-- mistake in a big pot no longer automatically outweighs an equally bad
-- preflop mistake. `ev_loss` (big blinds) stays as-is for the progress
-- chart and human-readable display; `ev_loss_pot` is the new column the
-- blunder tracker's rolling history is built from.

ALTER TABLE hero_decisions
    ADD COLUMN ev_loss_pot FLOAT NOT NULL DEFAULT 0;
