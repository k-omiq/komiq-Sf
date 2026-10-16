use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub(crate) struct Column {
    pub name: String,
    pub target: String,
    pub source: String,
    pub required: bool,
    pub generated: bool,
    pub identity: bool,
    pub repair: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ForeignKey {
    pub parent: String,
    pub column: String,
    pub parent_column: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Index {
    pub name: String,
    pub columns: Vec<String>,
    pub text_columns: Vec<String>,
    pub unique: bool,
    pub predicate: String,
    pub ascii: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Table {
    pub name: String,
    pub columns: Vec<Column>,
    pub keys: Vec<String>,
    pub fks: Vec<ForeignKey>,
    pub indexes: Vec<Index>,
    pub checks: Vec<String>,
}

pub(crate) fn tables() -> Vec<Table> {
    serde_json::from_str(include_str!("plan.json")).expect("checked-in import plan")
}

/// A table that only PostgreSQL has: a migration after the baseline created it, so no
/// SQLite file holds it, the import plan does not list it and the importer copies
/// nothing into it. It is empty until the server that runs on the imported database
/// writes to it.
pub(crate) struct PostgresOnly {
    pub name: &'static str,
    /// The version of the `migrations_pg` file that creates it. A target that an older
    /// release migrated does not have the table yet.
    pub since: i64,
    /// (column, parent table, parent column) of each of its foreign keys.
    pub fks: &'static [(&'static str, &'static str, &'static str)],
}

/// Every such table. The importer's inventory, its reset and the verifier's foreign-key
/// expectation read this list; the indexes are read from the migration files themselves.
pub(crate) const POSTGRES_ONLY: &[PostgresOnly] = &[
    PostgresOnly {
        name: "media_policy",
        since: 9,
        fks: &[],
    },
    PostgresOnly {
        name: "media_takedown",
        since: 9,
        fks: &[],
    },
    PostgresOnly {
        name: "media_object",
        since: 7,
        fks: &[],
    },
    PostgresOnly {
        name: "media_copy",
        since: 7,
        fks: &[],
    },
    PostgresOnly {
        name: "media_manifest",
        since: 7,
        fks: &[("copy_id", "media_copy", "id")],
    },
    PostgresOnly {
        name: "media_manifest_page",
        since: 7,
        fks: &[
            ("sha256", "media_object", "sha256"),
            ("copy_id", "media_copy", "id"),
        ],
    },
    PostgresOnly {
        name: "engine_numeric_state",
        since: 6,
        fks: &[],
    },
    PostgresOnly {
        name: "suwayomi_progress_detached",
        since: 6,
        fks: &[
            ("generation", "engine_generation", "id"),
            ("user_id", "users", "id"),
        ],
    },
    PostgresOnly {
        name: "scanlator_rating",
        since: 4,
        fks: &[("user_id", "users", "id")],
    },
    PostgresOnly {
        name: "engine_generation",
        since: 5,
        fks: &[],
    },
    PostgresOnly {
        name: "engine_manga_key",
        since: 5,
        fks: &[("generation", "engine_generation", "id")],
    },
    PostgresOnly {
        name: "engine_chapter_key",
        since: 5,
        fks: &[("generation", "engine_generation", "id")],
    },
];

pub(crate) fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

impl Table {
    pub fn copied(&self) -> impl Iterator<Item = &Column> {
        self.columns.iter().filter(|c| !c.generated)
    }
    pub fn sqlite_select(&self) -> String {
        format!(
            "SELECT {} FROM {}",
            self.copied()
                .map(|c| {
                    let name = quote(&c.name);
                    // CAST(REAL AS BLOB) goes through SQLite's 15-digit text formatter
                    // and loses f64 bits. Decode REAL directly through SQLx instead.
                    if c.target == "double precision" {
                    format!("typeof({name}), CASE WHEN typeof({name}) IN ('integer','real') THEN {name} ELSE CAST({name} AS BLOB) END")
                    } else {
                        format!("typeof({name}), CAST({name} AS BLOB)")
                    }
                })
                .collect::<Vec<_>>()
                .join(","),
            quote(&self.name)
        )
    }
    pub fn pg_select(&self) -> String {
        format!(
            "SELECT {} FROM public.{}",
            self.copied()
                .map(|c| {
                    let name = quote(&c.name);
                    // Read as text to avoid extending SQLx features or coupling request types.
                    if c.target == "bytea" {
                        format!("encode({name}, 'hex')")
                    } else {
                        format!("{name}::text")
                    }
                })
                .collect::<Vec<_>>()
                .join(","),
            quote(&self.name)
        )
    }
}
