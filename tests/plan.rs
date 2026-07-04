//! plan(): Diff → ordered Vec<Action>. Pure, so no filesystem needed — we assert the sequence.

use std::path::PathBuf;

use filesync::diff::{Change, Diff, Move};
use filesync::manifest::Kind;
use filesync::plan::{plan, Action};

fn p(s: &str) -> PathBuf {
    PathBuf::from(s)
}

fn pos(actions: &[Action], a: &Action) -> usize {
    actions.iter().position(|x| x == a).unwrap_or_else(|| panic!("missing action {a:?}"))
}

#[test]
fn empty_diff_plans_nothing() {
    assert!(plan(&Diff::default()).is_empty());
}

#[test]
fn phases_are_ordered_rename_delete_createdir_copy() {
    let d = Diff {
        added: vec![
            Change { rel: p("newdir"), kind: Kind::Dir },
            Change { rel: p("newdir/f.txt"), kind: Kind::File },
            Change { rel: p("top.txt"), kind: Kind::File },
        ],
        removed: vec![
            Change { rel: p("old"), kind: Kind::Dir },
            Change { rel: p("old/g.txt"), kind: Kind::File },
        ],
        changed: vec![Change { rel: p("edit.txt"), kind: Kind::File }],
        moved: vec![Move { from: p("was/x"), to: p("now/x") }],
        unchanged: 3,
    };
    let a = plan(&d);

    // every category is represented
    assert!(a.contains(&Action::CreateDir(p("newdir"))));
    assert!(a.contains(&Action::Rename { from: p("was/x"), to: p("now/x") }));
    assert!(a.contains(&Action::Copy(p("top.txt"))));
    assert!(a.contains(&Action::Copy(p("newdir/f.txt")))); // added file → copy
    assert!(a.contains(&Action::Copy(p("edit.txt")))); // changed → copy

    // cross-phase order: rename < delete < create-dir < copy
    let rn = pos(&a, &Action::Rename { from: p("was/x"), to: p("now/x") });
    let del_child = pos(&a, &Action::Delete(p("old/g.txt")));
    let del_parent = pos(&a, &Action::Delete(p("old")));
    let cd = pos(&a, &Action::CreateDir(p("newdir")));
    let cp = pos(&a, &Action::Copy(p("top.txt")));
    assert!(rn < del_child.min(del_parent), "rename before deletes");
    assert!(del_child.max(del_parent) < cd, "deletes before create-dirs (clears wrong-kind entries)");
    assert!(cd < cp, "create-dirs before copies");

    // within deletes: children before parents (so a dir is empty when removed)
    assert!(del_child < del_parent, "old/g.txt deleted before old/");
}

#[test]
fn added_directories_are_created_parents_first() {
    let d = Diff {
        added: vec![
            Change { rel: p("a/b/c"), kind: Kind::Dir },
            Change { rel: p("a"), kind: Kind::Dir },
            Change { rel: p("a/b"), kind: Kind::Dir },
        ],
        ..Diff::default()
    };
    let a = plan(&d);
    assert_eq!(
        a,
        vec![
            Action::CreateDir(p("a")),
            Action::CreateDir(p("a/b")),
            Action::CreateDir(p("a/b/c")),
        ]
    );
}

#[test]
fn a_pure_move_is_a_single_rename() {
    let d = Diff {
        moved: vec![Move { from: p("old/name"), to: p("new/name") }],
        ..Diff::default()
    };
    assert_eq!(plan(&d), vec![Action::Rename { from: p("old/name"), to: p("new/name") }]);
}
