mod add;

use super::Job;
use crate::services::download_client::DownloadItemState;

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
