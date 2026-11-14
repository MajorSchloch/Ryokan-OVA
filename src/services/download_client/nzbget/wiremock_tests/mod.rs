mod add;

use super::Job;
use crate::services::download_client::DownloadItemState;

#[test]
fn exact_copy_history_is_removed_without_an_import_path() {
    let job = Job {
        kind: "NZB".into(),
        name: "episode".into(),
        status: "DELETED/COPY".into(),
        dest_dir: "/completed/ryokan/episode".into(),
        ..Default::default()
    };
    let item = job.item(true, "/mount");
    assert_eq!(item.state_kind, DownloadItemState::Removed);
    assert_eq!(item.state, "DELETED/COPY");
    assert!(item.content_path.is_empty());
    assert!(!item.state_kind.is_complete());
    assert!(!item.state_kind.is_errored());
    assert!(!item.seeding_done);
    assert_ne!(job.state(false), DownloadItemState::Removed);
}

#[test]
fn other_history_warnings_still_require_review() {
    for status in [
        "WARNING/SPACE",
        "WARNING/PASSWORD",
        "WARNING/REPAIRABLE",
        "DELETED/DUPE",
        "DELETED/GOOD",
        "UNKNOWN",
    ] {
        let job = Job {
            kind: "NZB".into(),
            status: status.into(),
            ..Default::default()
        };
        assert_eq!(job.state(true), DownloadItemState::Warning, "{status}");
    }
}

#[test]
fn only_success_in_a_job_directory_is_ready_for_import() {
    let mut job = Job {
        kind: "NZB".into(),
        name: "episode".into(),
        status: "SUCCESS/UNPACK".into(),
        dest_dir: "/completed/ryokan/episode".into(),
        ..Default::default()
    };
    assert_eq!(job.item(true, "/mount").content_path, "/mount/episode");
    job.final_dir = "/completed/ryokan".into();
    let shared = job.item(true, "/mount");
    assert_eq!(shared.state_kind, DownloadItemState::Warning);
    assert!(shared.content_path.is_empty());

    job.final_dir.clear();
    job.status = "WARNING/SPACE".into();
    let warning = job.item(true, "/mount");
    assert_eq!(warning.state_kind, DownloadItemState::Warning);
    assert!(warning.content_path.is_empty());
}
