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
