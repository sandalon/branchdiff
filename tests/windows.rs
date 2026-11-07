//! Native Windows paths and filesystem behavior, exercised by Windows CI.
#![cfg(windows)]

mod common;
use branchdiff::diff::{self, Status};
use branchdiff::git;
use branchdiff::store::{Comment, Review, ReviewId, Side, Store};
use common::{git as run, real, Repo};

#[test]
fn git_diff_and_review_storage_handle_windows_paths_with_spaces_and_unicode() {
    let repo = Repo::new();
    let root = repo.main.parent().unwrap().join("feature worktree café");
    run(&repo.main, &["worktree", "add", "-q", "-b", "Feature/review", root.to_str().unwrap()]);
    let root = real(&root);
    let relative = "café file.rs";
    std::fs::write(root.join(relative), "old\n").unwrap();
    repo.commit_all(&root, "add file");
    std::fs::write(root.join(relative), "new\n").unwrap();

    let info = git::discover(&root).unwrap();
    assert_eq!(real(&info.toplevel), root);
    let base = git::merge_base(&root, "HEAD").unwrap();
    let files = diff::load(&root, &base).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!((files[0].path.as_str(), files[0].status), (relative, Status::Modified));
    assert_eq!(files[0].hunks[0].lines[1].text, "new");

    let store = Store::new(&info.common_dir);
    let id = ReviewId::Branch("Feature/CON".into());
    let review = Review {
        comments: vec![Comment {
            path: relative.into(),
            line: 1,
            side: Side::New,
            line_text: "new".into(),
            body: "Keep this note across a restart".into(),
        }],
        ..Review::default()
    };
    let version = store.load(&id).version;
    store.save(&id, &review, &version).unwrap();
    assert_eq!(Store::new(&info.common_dir).load(&id).review, review);
    let lower = ReviewId::Branch("feature/con".into());
    let lower_version = store.load(&lower).version;
    store.save(&lower, &Review::default(), &lower_version).unwrap();
    assert_eq!(store.load(&id).review, review, "case-insensitive paths must not merge review IDs");
    assert_ne!(store.path_for(&id), store.path_for(&lower));
}
