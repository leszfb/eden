// Copyright (c) 2017-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

//! Tests run against all bookmarks implementations.

#![deny(warnings)]

extern crate futures;
extern crate tempdir;
extern crate tokio_core;

extern crate bookmarks;
extern crate filebookmarks;
extern crate membookmarks;
extern crate storage_types;

use std::cell::RefCell;
use std::rc::Rc;

use futures::Stream;
use tempdir::TempDir;
use tokio_core::reactor::Core;

use bookmarks::BookmarksMut;
use filebookmarks::FileBookmarks;
use membookmarks::MemBookmarks;
use storage_types::Version;

fn basic<B>(bookmarks: B, core: &mut Core)
where
    B: BookmarksMut<Value = String>,
{
    let foo = "foo".to_string();
    let one = "1".to_string();
    let two = "2".to_string();
    let three = "3".to_string();

    assert_eq!(core.run(bookmarks.get(&foo)).unwrap(), None);

    let absent = Version::absent();
    let foo_v1 = core.run(bookmarks.set(&foo, &one, &absent))
        .unwrap()
        .unwrap();
    assert_eq!(
        core.run(bookmarks.get(&foo)).unwrap(),
        Some((one.clone(), foo_v1))
    );

    let foo_v2 = core.run(bookmarks.set(&foo, &two, &foo_v1))
        .unwrap()
        .unwrap();

    // Should fail due to version mismatch.
    assert_eq!(
        core.run(bookmarks.set(&foo, &three, &foo_v1)).unwrap(),
        None
    );

    assert_eq!(
        core.run(bookmarks.delete(&foo, &foo_v2)).unwrap().unwrap(),
        absent
    );
    assert_eq!(core.run(bookmarks.get(&foo)).unwrap(), None);

    // Even though bookmark doesn't exist, this should fail with a version mismatch.
    assert_eq!(core.run(bookmarks.delete(&foo, &foo_v2)).unwrap(), None);

    // Deleting it with the absent version should work.
    assert_eq!(
        core.run(bookmarks.delete(&foo, &absent)).unwrap().unwrap(),
        absent
    );
}

fn list<B>(bookmarks: B, core: &mut Core)
where
    B: BookmarksMut<Value = String>,
{
    let one = b"1";
    let two = b"2";
    let three = b"3";

    let _ = core.run(bookmarks.create(&one, &"foo".to_string()))
        .unwrap()
        .unwrap();
    let _ = core.run(bookmarks.create(&two, &"bar".to_string()))
        .unwrap()
        .unwrap();
    let _ = core.run(bookmarks.create(&three, &"baz".to_string()))
        .unwrap()
        .unwrap();

    let mut result = core.run(bookmarks.keys().collect()).unwrap();
    result.sort();

    let expected = vec![one, two, three];
    assert_eq!(result, expected);
}

fn persistence<F, B>(mut new_bookmarks: F, core: Rc<RefCell<Core>>)
where
    F: FnMut() -> B,
    B: BookmarksMut<Value = String>,
{
    let foo = "foo".to_string();
    let bar = "bar".to_string();

    let version = {
        let bookmarks = new_bookmarks();
        core.borrow_mut()
            .run(bookmarks.create(&foo, &bar))
            .unwrap()
            .unwrap()
    };

    let bookmarks = new_bookmarks();
    assert_eq!(
        core.borrow_mut().run(bookmarks.get(&foo)).unwrap(),
        Some((bar, version))
    );
}

macro_rules! bookmarks_test_impl {
    ($mod_name: ident => {
        state: $state: expr,
        new: $new_cb: expr,
        persistent: $persistent: expr,
    }) => {
        mod $mod_name {
            use super::*;

            #[test]
            fn test_basic() {
                let mut core = Core::new().unwrap();
                let state = $state;
                let bookmarks = $new_cb(&state, &mut core);
                basic(bookmarks, &mut core);
            }

            #[test]
            fn test_list() {
                let mut core = Core::new().unwrap();
                let state = $state;
                let bookmarks = $new_cb(&state, &mut core);
                list(bookmarks, &mut core);
            }

            #[test]
            fn test_persistence() {
                // Not all bookmark implementations support persistence.
                if $persistent {
                    let core = Rc::new(RefCell::new(Core::new().unwrap()));
                    let state = $state;
                    let new_bookmarks = {
                        let core = Rc::clone(&core);
                        move || {
                            $new_cb(&state, &mut *core.borrow_mut())
                        }
                    };
                    persistence(new_bookmarks, core);
                }
            }
        }
    }
}

bookmarks_test_impl! {
    membookmarks_test => {
        state: (),
        new: |_, _| MemBookmarks::new(),
        persistent: false,
    }
}

bookmarks_test_impl! {
    filebookmarks_test => {
        state: TempDir::new("filebookmarks_test").unwrap(),
        new: |dir: &TempDir, _| FileBookmarks::open(dir.as_ref()).unwrap(),
        persistent: true,
    }
}
