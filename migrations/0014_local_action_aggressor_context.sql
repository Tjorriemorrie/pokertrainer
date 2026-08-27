-- Lets the local-play action window carry position and c-bet context
-- (was the actor the preflop aggressor; is the flop bet they're facing from
-- that aggressor), so ActionFrequencyModel can condition on them for local
-- rows exactly like it does for freshly-walked gg_hands rows. Existing rows
-- default to "third seat, not applicable" (an acceptable one-time
-- undercount, same rationale as 0012_local_action_hand_no.sql).
ALTER TABLE local_opponent_actions
    ADD COLUMN position TEXT NOT NULL DEFAULT 'THIRD',
    ADD COLUMN was_preflop_aggressor BOOLEAN NOT NULL DEFAULT false,
    ADD COLUMN facing_cbet BOOLEAN NOT NULL DEFAULT false;

ALTER TABLE local_hero_actions
    ADD COLUMN position TEXT NOT NULL DEFAULT 'THIRD',
    ADD COLUMN was_preflop_aggressor BOOLEAN NOT NULL DEFAULT false,
    ADD COLUMN facing_cbet BOOLEAN NOT NULL DEFAULT false;
