-- Regional engines (fetch-only): which configured engine fetches a source's pages and
-- covers. A source with no row uses the owner engine. Only the route is stored here; a
-- regional engine's own numeric ids are resolved in memory and never written down.
CREATE TABLE "source_engine_route" (
    "source_id" text NOT NULL,
    "engine_id" text NOT NULL,
    -- Strict: answer 503 while the regional engine cannot serve, instead of falling
    -- back to the owner engine.
    "strict" boolean NOT NULL DEFAULT false,
    "set_at" timestamptz NOT NULL,
    "set_by" text NULL,
    CONSTRAINT "source_engine_route_pkey" PRIMARY KEY ("source_id"),
    CONSTRAINT "source_engine_route_engine_id_ck" CHECK (
        "engine_id" ~ '^[a-z0-9][a-z0-9-]{0,31}$' AND "engine_id" <> 'owner'
    )
);
