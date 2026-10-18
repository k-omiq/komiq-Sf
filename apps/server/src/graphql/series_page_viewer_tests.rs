//! `seriesPageViewer` laid over an anonymous `seriesPage` is the signed-in `seriesPage`,
//! and anonymous answers carry no viewer data (F8: public reads go out anonymous).
use super::tests::{
    drain_work_chapter_fixture, exec, seed_work_chapter_graphql_fixture, setup_full,
};
use super::*;
use serde_json::{json, Value};

const PAGE: &str = r#"seriesPage(workId:"{work}", includeFederated:true) {
    workId
    viewerProgress { revision throughEntryId }
    series { id isMarked libraryStatus isFavorite }
    spineChapters { id read lastPageRead }
    sources { series { id isMarked libraryStatus isFavorite } chapters { id read lastPageRead } }
    aggregated { entryId key readAnywhere }
}"#;

const VIEWER: &str = r#"seriesPageViewer(workId:"{work}") {
    workId
    viewerProgress { revision throughEntryId }
    library { seriesId isMarked libraryStatus isFavorite }
    chapters { sourceType chapterId read lastPageRead }
    aggregated { entryId key }
}"#;

async fn query(schema: &ApiSchema, text: &str, token: Option<&str>) -> Value {
    let response = exec(schema, text, token, "series-page-viewer").await;
    assert!(response.errors.is_empty(), "{:?}", response.errors);
    response.data.into_json().unwrap()
}

fn page(work: &str) -> String {
    format!("{{ {} }}", PAGE.replace("{work}", work))
}

fn viewer(work: &str) -> String {
    format!("{{ {} }}", VIEWER.replace("{work}", work))
}

/// What the reader does with the two answers (`GraphQLBackend.seriesPage`).
fn lay_over(mut page: Value, viewer: &Value) -> Value {
    page["viewerProgress"] = viewer["viewerProgress"].clone();
    let flags = |series: &mut Value| {
        if series.is_null() {
            return;
        }
        let found = viewer["library"]
            .as_array()
            .unwrap()
            .iter()
            .find(|flags| flags["seriesId"] == series["id"]);
        if let Some(flags) = found {
            for field in ["isMarked", "libraryStatus", "isFavorite"] {
                series[field] = flags[field].clone();
            }
        }
    };
    let chapters = |chapters: &mut Value, source_type: &str| {
        for chapter in chapters.as_array_mut().unwrap() {
            let found = viewer["chapters"].as_array().unwrap().iter().find(|state| {
                state["sourceType"] == source_type && state["chapterId"] == chapter["id"]
            });
            if let Some(state) = found {
                chapter["read"] = state["read"].clone();
                chapter["lastPageRead"] = state["lastPageRead"].clone();
            }
        }
    };
    flags(&mut page["series"]);
    chapters(&mut page["spineChapters"], "mangadex");
    for source in page["sources"].as_array_mut().unwrap() {
        flags(&mut source["series"]);
        chapters(&mut source["chapters"], "suwayomi");
    }
    for row in page["aggregated"].as_array_mut().unwrap() {
        row["readAnywhere"] = json!(viewer["aggregated"]
            .as_array()
            .unwrap()
            .iter()
            .any(|read| read["entryId"] == row["entryId"] && read["key"] == row["key"]));
    }
    page
}

/// Every viewer field of an anonymous page is at its signed-out value.
fn assert_no_viewer_data(page: &Value) {
    assert!(page["viewerProgress"].is_null(), "{page}");
    let mut series = vec![&page["series"]];
    let mut chapters: Vec<&Value> = page["spineChapters"].as_array().unwrap().iter().collect();
    for source in page["sources"].as_array().unwrap() {
        series.push(&source["series"]);
        chapters.extend(source["chapters"].as_array().unwrap());
    }
    for series in series.into_iter().filter(|s| !s.is_null()) {
        assert_eq!(series["isMarked"], false, "{series}");
        assert_eq!(series["isFavorite"], false, "{series}");
        assert!(series["libraryStatus"].is_null(), "{series}");
    }
    for chapter in chapters {
        assert_eq!(chapter["read"], false, "{chapter}");
        assert_eq!(chapter["lastPageRead"], 0, "{chapter}");
    }
    for row in page["aggregated"].as_array().unwrap() {
        assert_eq!(row["readAnywhere"], false, "{row}");
    }
}

#[tokio::test]
async fn the_viewer_overlay_on_an_anonymous_page_is_the_signed_in_page() {
    for ready in [false, true] {
        let (schema, pool) = setup_full(100).await;
        let work = seed_work_chapter_graphql_fixture(&pool).await;
        if ready {
            drain_work_chapter_fixture(&pool).await;
        }
        let mut revision = 0;
        for (id, page, read) in [
            ("entry-one", 7, false),
            ("entry-two", 0, true),
            ("9201", 3, false),
            ("9203", 12, true),
        ] {
            let saved = query(&schema, &format!(
                r#"mutation {{ saveProgress(chapterId:"{id}",workId:"{work}",lastPageRead:{page},read:{read},revision:{revision}) {{ revision }} }}"#,
            ), Some("bobtok")).await;
            revision = saved["saveProgress"]["revision"]
                .as_i64()
                .unwrap_or(revision);
        }
        query(
            &schema,
            &format!(
                r#"mutation {{
                    mark(seriesId:"{work}", marked:true) {{ id }}
                    setLibraryStatus(seriesId:"{work}", status:"reading") {{ id }}
                    setFavorite(seriesId:"779", favorite:true) {{ id }}
                }}"#
            ),
            Some("bobtok"),
        )
        .await;

        let signed = query(&schema, &page(&work), Some("bobtok")).await["seriesPage"].clone();
        let anonymous = query(&schema, &page(&work), None).await["seriesPage"].clone();
        let overlay =
            query(&schema, &viewer(&work), Some("bobtok")).await["seriesPageViewer"].clone();
        let case = format!("ready={ready}");

        // The fixture really carries viewer data, or the comparison proves nothing.
        assert_eq!(signed["series"]["isMarked"], true, "{case}");
        assert_eq!(signed["series"]["libraryStatus"], "reading", "{case}");
        let source = signed["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|source| source["series"]["id"] == "779")
            .unwrap_or_else(|| panic!("{case}: no source 779 in {signed}"));
        assert_eq!(source["series"]["isFavorite"], true, "{case}");
        assert!(
            signed["viewerProgress"]["revision"].as_i64() > Some(0),
            "{case}"
        );
        let read_copies = signed["spineChapters"]
            .as_array()
            .unwrap()
            .iter()
            .chain(source["chapters"].as_array().unwrap())
            .filter(|c| c["read"] == true)
            .count();
        assert!(read_copies >= 2, "{case}: {signed}");
        assert!(
            signed["aggregated"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["readAnywhere"] == true),
            "{case}"
        );

        assert_no_viewer_data(&anonymous);
        assert_eq!(overlay["workId"], signed["workId"], "{case}");
        assert_eq!(lay_over(anonymous, &overlay), signed, "{case}");

        // Signed out, the overlay is empty: no viewer, nothing to lay over.
        let empty = query(&schema, &viewer(&work), None).await["seriesPageViewer"].clone();
        assert_eq!(
            empty,
            json!({
                "workId": work,
                "viewerProgress": null,
                "library": [],
                "chapters": [],
                "aggregated": [],
            }),
            "{case}"
        );
    }
}

/// The anonymous catalogue reads a signed-in reader now sends without its token carry
/// none of that viewer's data.
#[tokio::test]
async fn anonymous_catalogue_reads_carry_no_viewer_data() {
    let (schema, pool) = setup_full(100).await;
    let work = seed_work_chapter_graphql_fixture(&pool).await;
    query(
        &schema,
        &format!(
            r#"mutation {{
                mark(seriesId:"{work}", marked:true) {{ id }}
                setFavorite(seriesId:"{work}", favorite:true) {{ id }}
                setLibraryStatus(seriesId:"{work}", status:"reading") {{ id }}
            }}"#
        ),
        Some("bobtok"),
    )
    .await;
    let text = r#"{ search(query:"Work Chapter Fixture") { items { id isMarked isFavorite libraryStatus } } }"#;
    let signed = query(&schema, text, Some("bobtok")).await;
    let found = |data: &Value| {
        data["search"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == work)
            .cloned()
            .unwrap_or_else(|| panic!("{work} not in {data}"))
    };
    assert_eq!(found(&signed)["isMarked"], true);
    let anonymous = found(&query(&schema, text, None).await);
    assert_eq!(
        anonymous,
        json!({ "id": work, "isMarked": false, "isFavorite": false, "libraryStatus": null })
    );
}
