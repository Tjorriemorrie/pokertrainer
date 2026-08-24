-- Range model — add stack-depth bucket and per-node sample counts to
-- contextual_ranges so ranges can be keyed by (node, profile, stack bucket)
-- and the <30-hand population fallback can be applied.

ALTER TABLE contextual_ranges
    ADD COLUMN stack_bucket SMALLINT NOT NULL DEFAULT 25
        CHECK (stack_bucket IN (10, 15, 25)),
    ADD COLUMN sample_count INT NOT NULL DEFAULT 0;

ALTER TABLE contextual_ranges
    DROP CONSTRAINT contextual_ranges_node_profile_id_key;

ALTER TABLE contextual_ranges
    ADD CONSTRAINT contextual_ranges_node_profile_id_stack_bucket_key
    UNIQUE (node, profile_id, stack_bucket);
