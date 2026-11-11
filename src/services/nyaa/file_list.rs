//! Bounded published-file evidence for otherwise unmarked season packs.
use std::sync::LazyLock;

use ::scraper::{ElementRef, Html, Selector};

use super::{HTTP_CLIENT, NYAA_CONCURRENCY, VIEW_BODY_CAP, view_page_url};

const FILE_CAP: usize = 4096;
static PANELS: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".panel").unwrap());
static TITLES: LazyLock<Selector> = LazyLock::new(|| Selector::parse("h3.panel-title").unwrap());
static MAGNETS: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("a.card-footer-item[href^='magnet:']").unwrap());
static TREES: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".torrent-file-list.panel-body").unwrap());
static LEAVES: LazyLock<Selector> = LazyLock::new(|| Selector::parse("li").unwrap());

pub(crate) struct PublishedFiles {
    pub title: String,
    pub files: Vec<String>,
    pub description: String,
}

/// This adds no client job and trusts no off-origin listing URL. The page's
/// magnet must identify the same source hash as the search result.
pub(crate) async fn fetch_published_files(
    link: &str,
    expected_hash: &str,
) -> Result<PublishedFiles, String> {
    let page = view_page_url(link).ok_or("Not a Nyaa listing page")?;
    if expected_hash.len() != 40 || !expected_hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("Published files need a source infohash".into());
    }
    let _permit = NYAA_CONCURRENCY
        .acquire()
        .await
        .map_err(|_| "Nyaa is unavailable")?;
    let response = HTTP_CLIENT
        .get(&page)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| format!("Nyaa file-list fetch failed: {}", e.without_url()))?;
    let body = crate::services::http_body::read_capped(response, VIEW_BODY_CAP).await?;
    let html = String::from_utf8_lossy(&body).into_owned();
    let hash = expected_hash.to_string();
    tokio::task::spawn_blocking(move || parse(&html, &page, &hash))
        .await
        .map_err(|_| "Nyaa file-list parsing failed".to_string())?
}

fn component(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 4096
        && !matches!(name, "." | "..")
        && !name.contains(['/', '\\', '\0'])
}

fn class(node: ElementRef<'_>, name: &str) -> bool {
    node.value().classes().any(|value| value == name)
}

fn trusted_tree(tree: ElementRef<'_>) -> bool {
    let Some(panel) = tree.parent().and_then(ElementRef::wrap) else {
        return false;
    };
    if !class(panel, "panel") {
        return false;
    }
    if panel.ancestors().filter_map(ElementRef::wrap).any(|node| {
        class(node, "panel")
            || matches!(
                node.value().attr("id"),
                Some("torrent-description" | "comments" | "collapse-comments")
            )
    }) {
        return false;
    }
    panel
        .children()
        .filter_map(ElementRef::wrap)
        .filter(|node| class(*node, "panel-heading"))
        .any(|heading| {
            heading
                .select(&TITLES)
                .any(|title| title.text().collect::<String>().trim() == "File list")
        })
}

fn parse(html: &str, _page: &str, hash: &str) -> Result<PublishedFiles, String> {
    let document = Html::parse_document(html);
    let header = document
        .select(&PANELS)
        .next()
        .ok_or("Nyaa listing header is unavailable")?;
    let heading = header
        .children()
        .filter_map(ElementRef::wrap)
        .find(|node| class(*node, "panel-heading"))
        .ok_or("Nyaa listing heading is unavailable")?;
    let title = heading
        .select(&TITLES)
        .next()
        .map(|title| title.text().collect::<String>())
        .ok_or("Nyaa listing title is unavailable")?;
    if title.trim().is_empty() {
        return Err("Nyaa listing title is unavailable".into());
    }
    let footer = header
        .children()
        .filter_map(ElementRef::wrap)
        .find(|node| class(*node, "panel-footer"))
        .ok_or("Nyaa listing footer is unavailable")?;
    let magnet = footer
        .select(&MAGNETS)
        .next()
        .and_then(|node| node.value().attr("href"))
        .ok_or("Nyaa listing source identity is unavailable")?;
    if !super::extract_hash(magnet).eq_ignore_ascii_case(hash) {
        return Err("Nyaa listing source identity changed".into());
    }
    let trees: Vec<_> = document
        .select(&TREES)
        .filter(|tree| trusted_tree(*tree))
        .collect();
    if trees.len() != 1 {
        return Err("A unique published file list is unavailable".into());
    }
    if trees[0]
        .select(&TREES)
        .any(|nested| nested.id() != trees[0].id())
    {
        return Err("Nested published file lists are ambiguous".into());
    }
    let mut files = Vec::new();
    for item in trees[0].select(&LEAVES) {
        let file = item.children().filter_map(ElementRef::wrap).any(|node| {
            node.value().name() == "i" && node.value().classes().any(|class| class == "fa-file")
        });
        if !file {
            continue;
        }
        // Direct text excludes nested folder descendants and the size span.
        let name = item
            .children()
            .filter_map(|n| n.value().as_text())
            .map(|text| text.text.as_ref())
            .collect::<String>();
        let name = name.trim();
        if !component(name) {
            return Err("Invalid published filename".into());
        }
        let mut folders = Vec::new();
        for ancestor in item.ancestors().filter_map(ElementRef::wrap) {
            if ancestor.value().name() != "li" {
                continue;
            }
            if let Some(folder) = ancestor
                .children()
                .filter_map(ElementRef::wrap)
                .find(|node| {
                    node.value().name() == "a" && node.value().classes().any(|c| c == "folder")
                })
            {
                let folder = folder.text().collect::<String>();
                let folder = folder.trim();
                if !component(folder) || folders.len() == 16 {
                    return Err("Invalid published folder".into());
                }
                folders.push(folder.to_string());
            }
        }
        folders.reverse();
        folders.push(name.to_string());
        let joined = folders.join("/");
        if joined.len() > 4096 {
            return Err("Published path exceeds the limit".into());
        }
        files.push(joined);
        if files.len() > FILE_CAP {
            return Err("Published file list exceeds the limit".into());
        }
    }
    if files.is_empty() {
        return Err("Published file list is unavailable".into());
    }
    Ok(PublishedFiles {
        title: title.trim().to_string(),
        files,
        description: crate::services::source_description::extract_description_text(html),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(body: &str) -> String {
        format!(
            r#"<div class="panel"><div class="panel-heading"><h3 class="panel-title">Show</h3></div><div class="panel-footer"><a class="card-footer-item" href="magnet:?xt=urn:btih:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa">Magnet</a></div></div>{body}"#
        )
    }

    fn tree(body: &str) -> String {
        format!(
            r#"<div class="panel"><div class="panel-heading"><h3 class="panel-title">File list</h3></div><div class="torrent-file-list panel-body">{body}</div></div>"#
        )
    }

    #[test]
    fn published_files_keep_nested_paths_without_folder_duplicates_or_sizes() {
        let html = page(&tree(
            r#"<ul><li><a class="folder"><i class="fa fa-folder"></i>Show</a><ul><li><i class="fa fa-file"></i>Show - 01.mkv <span class="file-size">(1 GiB)</span></li><li><a class="folder">Specials</a><ul><li><i class="fa fa-file"></i>Show - NCOP.mkv <span class="file-size">(20 MiB)</span></li></ul></li></ul></li></ul>"#,
        ));
        let parsed = parse(&html, "https://nyaa.si/view/1", &"a".repeat(40)).unwrap();
        assert_eq!(
            parsed.files,
            ["Show/Show - 01.mkv", "Show/Specials/Show - NCOP.mkv"]
        );
    }

    #[test]
    fn published_files_require_matching_hash_and_available_safe_tree() {
        assert!(parse(&page(""), "https://nyaa.si/view/1", &"b".repeat(40)).is_err());
        assert!(parse(&page(""), "https://nyaa.si/view/1", &"a".repeat(40)).is_err());
        let unsafe_name = page(&tree(
            r#"<ul><li><i class="fa fa-file"></i>../Show.mkv</li></ul>"#,
        ));
        assert!(parse(&unsafe_name, "https://nyaa.si/view/1", &"a".repeat(40)).is_err());
        let oversized = page(&tree(&format!(
            r#"<ul><li><a class="folder">{}</a><ul><li><i class="fa fa-file"></i>Show - 01.mkv</li></ul></li></ul>"#,
            "a".repeat(4090)
        )));
        assert!(parse(&oversized, "https://nyaa.si/view/1", &"a".repeat(40)).is_err());
    }

    #[test]
    fn published_tree_ignores_forged_user_content_and_requires_one_real_tree() {
        let valid = tree(r#"<ul><li><i class="fa fa-file"></i>Show - 01.mkv</li></ul>"#);
        let forged = tree(r#"<ul><li><i class="fa fa-file"></i>Foreign - 99.mkv</li></ul>"#);
        let description =
            format!(r#"<div class="panel"><div id="torrent-description">{forged}</div></div>"#);
        let comments = format!(r#"<div id="comments">{forged}</div>"#);
        let parsed = parse(
            &page(&format!("{description}{valid}{comments}")),
            "https://nyaa.si/view/1",
            &"a".repeat(40),
        )
        .unwrap();
        assert_eq!(parsed.files, ["Show - 01.mkv"]);
        assert!(
            parse(
                &page(&description),
                "https://nyaa.si/view/1",
                &"a".repeat(40)
            )
            .is_err()
        );
        assert!(
            parse(
                &page(&tree(&format!("<ul></ul>{forged}"))),
                "https://nyaa.si/view/1",
                &"a".repeat(40)
            )
            .is_err()
        );
        assert!(
            parse(
                &page(&format!("{valid}{valid}")),
                "https://nyaa.si/view/1",
                &"a".repeat(40)
            )
            .is_err()
        );
        let missing_header_hash = format!(
            r#"<div class="panel"><div class="panel-heading"><h3 class="panel-title">Show</h3></div></div><div id="torrent-description"><a class="card-footer-item" href="magnet:?xt=urn:btih:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa">Fake identity</a></div>{valid}"#
        );
        assert!(
            parse(
                &missing_header_hash,
                "https://nyaa.si/view/1",
                &"a".repeat(40)
            )
            .is_err()
        );
    }
}
