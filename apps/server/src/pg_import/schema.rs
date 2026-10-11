use anyhow::{ensure, Result};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use std::collections::BTreeMap;

// Include all non-system schemas, including importer state and extension objects.
// Catalog OIDs and USER trigger enabled state are deliberately absent: OIDs are
// not definitions, and table loads disable USER triggers until publication.
// Internal FK triggers are never disabled by the loader. Their stable constraint
// identities, implementation and enabled state belong in the fingerprint.
// Collations use schema/name; policy roles use names and a boolean PUBLIC marker.
const SECTIONS: &[(&str, &str)] = &[
    ("tables", "SELECT json_build_array(n.nspname,c.relname,c.relkind,c.relpersistence,c.relrowsecurity,c.relforcerowsecurity)::text FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE c.relkind IN ('r','p','f','m','v')"),
    ("columns", "SELECT json_build_array(n.nspname,c.relname,a.attnum,a.attname,format_type(a.atttypid,a.atttypmod),a.attnotnull,pg_get_expr(d.adbin,d.adrelid),a.attidentity,a.attgenerated,cn.nspname,co.collname)::text FROM pg_attribute a JOIN pg_class c ON c.oid=a.attrelid JOIN pg_namespace n ON n.oid=c.relnamespace LEFT JOIN pg_attrdef d ON d.adrelid=a.attrelid AND d.adnum=a.attnum LEFT JOIN pg_collation co ON co.oid=a.attcollation LEFT JOIN pg_namespace cn ON cn.oid=co.collnamespace WHERE c.relkind IN ('r','p','f','m','v') AND a.attnum>0 AND NOT a.attisdropped"),
    ("policies", "SELECT json_build_array(n.nspname,c.relname,p.polname,p.polcmd,p.polpermissive,ARRAY(SELECT json_build_array(r.role_oid=0,pr.rolname)::text FROM unnest(p.polroles) r(role_oid) LEFT JOIN pg_roles pr ON pr.oid=r.role_oid ORDER BY json_build_array(r.role_oid=0,pr.rolname)::text COLLATE \"C\"),pg_get_expr(p.polqual,p.polrelid),pg_get_expr(p.polwithcheck,p.polrelid))::text FROM pg_policy p JOIN pg_class c ON c.oid=p.polrelid JOIN pg_namespace n ON n.oid=c.relnamespace WHERE true"),
    ("constraints", "SELECT json_build_array(n.nspname,c.relname,t.typname,k.conname,pg_get_constraintdef(k.oid,false))::text FROM pg_constraint k JOIN pg_namespace n ON n.oid=k.connamespace LEFT JOIN pg_class c ON c.oid=k.conrelid LEFT JOIN pg_type t ON t.oid=k.contypid WHERE true"),
    ("indexes", "SELECT json_build_array(n.nspname,c.relname,x.relname,pg_get_indexdef(i.indexrelid))::text FROM pg_index i JOIN pg_class c ON c.oid=i.indrelid JOIN pg_class x ON x.oid=i.indexrelid JOIN pg_namespace n ON n.oid=c.relnamespace WHERE true"),
    ("triggers", "SELECT json_build_array(n.nspname,c.relname,t.tgname,pg_get_triggerdef(t.oid,false))::text FROM pg_trigger t JOIN pg_class c ON c.oid=t.tgrelid JOIN pg_namespace n ON n.oid=c.relnamespace WHERE NOT t.tgisinternal"),
    ("foreign_key_triggers", "SELECT json_build_array(n.nspname,c.relname,k.conname,tn.nspname,tc.relname,t.tgtype,t.tgenabled,t.tgdeferrable,t.tginitdeferred,pn.nspname,p.proname,pg_get_function_identity_arguments(p.oid))::text FROM pg_constraint k JOIN pg_class c ON c.oid=k.conrelid JOIN pg_namespace n ON n.oid=c.relnamespace JOIN pg_trigger t ON t.tgconstraint=k.oid JOIN pg_class tc ON tc.oid=t.tgrelid JOIN pg_namespace tn ON tn.oid=tc.relnamespace JOIN pg_proc p ON p.oid=t.tgfoid JOIN pg_namespace pn ON pn.oid=p.pronamespace WHERE k.contype='f' AND t.tgisinternal"),
    ("functions", "SELECT json_build_array(n.nspname,p.proname,pg_get_function_identity_arguments(p.oid),p.prokind,CASE WHEN p.prokind <> 'a' THEN pg_get_functiondef(p.oid) ELSE p.prosrc END,p.provolatile,p.proisstrict,p.prosecdef,p.proconfig)::text FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE true"),
    ("sequences", "SELECT json_build_array(n.nspname,s.relname,d.deptype,onsp.nspname,c.relname,a.attname,q.seqtypid::regtype::text,q.seqstart,q.seqincrement,q.seqmax,q.seqmin,q.seqcache,q.seqcycle)::text FROM pg_class s JOIN pg_namespace n ON n.oid=s.relnamespace JOIN pg_sequence q ON q.seqrelid=s.oid LEFT JOIN pg_depend d ON d.classid='pg_class'::regclass AND d.objid=s.oid AND d.refclassid='pg_class'::regclass AND d.deptype IN ('a','i') LEFT JOIN pg_class c ON c.oid=d.refobjid LEFT JOIN pg_namespace onsp ON onsp.oid=c.relnamespace LEFT JOIN pg_attribute a ON a.attrelid=c.oid AND a.attnum=d.refobjsubid WHERE s.relkind='S'"),
    ("extensions", "SELECT json_build_array(e.extname,e.extversion,n.nspname)::text FROM pg_extension e JOIN pg_namespace n ON n.oid=e.extnamespace WHERE true"),
    ("text_search", "SELECT json_build_array(n.nspname,c.cfgname,pn.nspname,p.prsname,pg_get_functiondef(p.prsstart),pg_get_functiondef(p.prstoken),pg_get_functiondef(p.prsend),pg_get_functiondef(p.prsheadline),pg_get_functiondef(p.prslextype),m.maptokentype,m.mapseqno,dn.nspname,d.dictname,d.dictinitoption,tn.nspname,t.tmplname,CASE WHEN t.tmplinit<>0 THEN pg_get_functiondef(t.tmplinit) END,pg_get_functiondef(t.tmpllexize))::text FROM pg_ts_config c JOIN pg_namespace n ON n.oid=c.cfgnamespace JOIN pg_ts_parser p ON p.oid=c.cfgparser JOIN pg_namespace pn ON pn.oid=p.prsnamespace LEFT JOIN pg_ts_config_map m ON m.mapcfg=c.oid LEFT JOIN pg_ts_dict d ON d.oid=m.mapdict LEFT JOIN pg_namespace dn ON dn.oid=d.dictnamespace LEFT JOIN pg_ts_template t ON t.oid=d.dicttemplate LEFT JOIN pg_namespace tn ON tn.oid=t.tmplnamespace WHERE c.oid='pg_catalog.simple'::regconfig"),
];

pub(super) async fn fingerprint(conn: &mut PgConnection) -> Result<BTreeMap<String, String>> {
    let mut sections = BTreeMap::new();
    for (name, query) in SECTIONS {
        let sql = if matches!(*name, "extensions" | "text_search") {
            (*query).to_owned()
        } else {
            format!("{query} AND n.nspname <> 'information_schema' AND n.nspname !~ '^pg_'")
        };
        let mut definitions: Vec<String> = sqlx::query_scalar(&sql).fetch_all(&mut *conn).await?;
        // Sort bytes in Rust, independent of database collation. Frame each row so
        // adjacent definitions cannot alias through concatenation.
        definitions.sort();
        let mut hash = Sha256::new();
        hash.update(b"komika-pg-schema-v1\0");
        hash.update(name.as_bytes());
        for definition in definitions {
            hash.update((definition.len() as u64).to_be_bytes());
            hash.update(definition.as_bytes());
        }
        sections.insert((*name).to_owned(), hex::encode(hash.finalize()));
    }
    Ok(sections)
}

pub(super) async fn verify(conn: &mut PgConnection) -> Result<()> {
    foreign_key_triggers(conn).await?;
    let expected: Option<sqlx::types::Json<BTreeMap<String, String>>> = sqlx::query_scalar(
        "SELECT schema_hashes FROM public.komika_import_state WHERE singleton=true",
    )
    .fetch_optional(&mut *conn)
    .await?;
    let expected = expected
        .ok_or_else(|| anyhow::anyhow!("schema fingerprint is absent"))?
        .0;
    let actual = fingerprint(conn).await?;
    for (section, _) in SECTIONS {
        ensure!(
            expected.get(*section) == actual.get(*section),
            "PostgreSQL schema fingerprint differs: {section} section"
        );
    }
    ensure!(
        expected.len() == actual.len(),
        "schema fingerprint sections differ"
    );
    Ok(())
}

pub(super) async fn foreign_key_triggers(conn: &mut PgConnection) -> Result<()> {
    // The unpartitioned baseline has four implementation triggers per FK:
    // child INSERT/UPDATE and parent DELETE/UPDATE. Generated trigger names
    // contain OIDs, so compare the inventory by constraint and event instead.
    let invalid: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (SELECT k.oid FROM pg_constraint k \
         JOIN pg_namespace n ON n.oid=k.connamespace \
         LEFT JOIN pg_trigger t ON t.tgconstraint=k.oid \
         WHERE n.nspname='public' AND k.contype='f' GROUP BY k.oid \
         HAVING count(t.oid)<>4 OR count(DISTINCT (t.tgrelid,t.tgtype,t.tgfoid))<>4 \
           OR NOT bool_and(t.tgisinternal AND t.tgenabled='O')) invalid",
    )
    .fetch_one(conn)
    .await?;
    ensure!(
        invalid == 0,
        "foreign-key implementation triggers are missing or not in normal enabled mode"
    );
    Ok(())
}

pub(super) async fn search_contract(conn: &mut PgConnection) -> Result<()> {
    // A freshly fingerprinted target could already have altered system search
    // objects. Require the PostgreSQL 18 simple-parser contract independently
    // of the generated expression and the verifier's rebuild equivalent.
    let valid: bool = sqlx::query_scalar(
        "SELECT COALESCE(c.cfgparser=(SELECT oid FROM pg_ts_parser \
            WHERE prsnamespace='pg_catalog'::regnamespace AND prsname='default') \
          AND (SELECT array_agg(m.maptokentype ORDER BY m.maptokentype,m.mapseqno) \
               FROM pg_ts_config_map m WHERE m.mapcfg=c.oid) \
            = ARRAY[1,2,3,4,5,6,7,8,9,10,11,15,16,17,18,19,20,21,22] \
          AND NOT EXISTS (SELECT 1 FROM pg_ts_config_map m WHERE m.mapcfg=c.oid \
            AND (m.mapseqno<>1 OR m.mapdict<>'pg_catalog.simple'::regdictionary)) \
          AND d.dictinitoption IS NULL AND tn.nspname='pg_catalog' AND t.tmplname='simple' \
          AND to_tsvector('pg_catalog.simple'::regconfig,'running cafe リゼロ the foo-bar 1.2') \
            = $$'1.2':8 'bar':7 'cafe':2 'foo':6 'foo-bar':5 'running':1 'the':4 'リゼロ':3$$::tsvector, false) \
         FROM pg_ts_config c JOIN pg_ts_dict d ON d.oid='pg_catalog.simple'::regdictionary \
         JOIN pg_ts_template t ON t.oid=d.dicttemplate JOIN pg_namespace tn ON tn.oid=t.tmplnamespace \
         WHERE c.oid='pg_catalog.simple'::regconfig",
    )
    .fetch_one(conn)
    .await?;
    ensure!(valid, "PostgreSQL simple text-search contract differs");
    Ok(())
}
