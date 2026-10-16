-- The policy epoch: bumped in the same transaction as any change that can make a
-- public object unservable (`media_policy::bump`). No seed row: an absent row is
-- epoch 0, so an import that resets this table starts from the empty state. The
-- next bump moves to at least the current time in milliseconds, so the epoch never
-- goes back after a reset.
CREATE TABLE "media_policy" (
    "singleton" boolean NOT NULL DEFAULT true,
    "epoch" bigint NOT NULL,
    CONSTRAINT "media_policy_pkey" PRIMARY KEY ("singleton"),
    CONSTRAINT "media_policy_ck_0" CHECK (singleton)
);
-- Operator takedowns. A copy row recorded for a work takedown names that work in
-- `via_work`, so lifting the work takedown lifts them too. A populated table
-- prevents reverse export: a rollback that dropped it would serve the content again.
CREATE TABLE "media_takedown" (
    "subject_kind" text NOT NULL,
    "subject_id" text NOT NULL,
    "epoch" bigint NOT NULL,
    "reason" text NOT NULL,
    "created_by" text NULL,
    "created_at" timestamptz NOT NULL,
    "via_work" text NULL,
    CONSTRAINT "media_takedown_pkey" PRIMARY KEY ("subject_kind", "subject_id"),
    CONSTRAINT "media_takedown_ck_0" CHECK (subject_kind IN ('work', 'copy'))
);
CREATE INDEX idx_media_takedown_via_work ON media_takedown (via_work);
