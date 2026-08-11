#![cfg(unix)]

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
#[cfg(target_os = "linux")]
use std::os::unix::fs::PermissionsExt;
#[cfg(target_os = "linux")]
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use marion_supervisor::native_bootstrap::{DirectNativeRequestContext, context_hash};
use marion_supervisor::serve::own_uid;
use marion_supervisor::socket::{Acquired, acquire, socket_paths};
use marion_testsupport::scratch;

fn request_context(selector: &str, tail: Vec<OsString>) -> DirectNativeRequestContext {
    DirectNativeRequestContext::new(
        PathBuf::from(OsString::from_vec(b"/canonical/project-\xff".to_vec())),
        OsString::from(selector),
        tail,
        OsString::from_vec(b"xterm-\xfe".to_vec()),
        7,
    )
}

#[cfg(target_os = "linux")]
#[test]
fn native_bootstrap_listener_is_a_private_sibling_of_the_project_socket() {
    let work = scratch("native-bootstrap-listener");
    let state = work.join("state");
    let project = work.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let paths = socket_paths(&state, &project, own_uid());
    assert_eq!(paths.native_bootstrap().parent(), paths.socket().parent());
    assert_ne!(paths.native_bootstrap(), paths.socket());

    let Acquired::Serving(serving) = acquire(&paths).expect("bind both project listeners") else {
        panic!("fresh project unexpectedly dialed an existing supervisor")
    };
    assert_eq!(serving.native_bootstrap_path(), paths.native_bootstrap());
    assert_eq!(
        std::fs::metadata(paths.native_bootstrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600,
    );
    let client = UnixStream::connect(paths.native_bootstrap()).expect("dial native sibling");
    let (accepted, _) = serving
        .native_bootstrap_listener()
        .expect("supported platforms expose the native listener")
        .accept()
        .expect("accept native sibling connection");
    drop((client, accepted));
}

#[cfg(not(target_os = "linux"))]
#[test]
fn macos_fails_closed_without_a_native_listener_or_socket_artifact() {
    let work = scratch("native-bootstrap-disabled-macos");
    let state = work.join("state");
    let project = work.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let paths = socket_paths(&state, &project, own_uid());

    let Acquired::Serving(serving) = acquire(&paths).expect("bind the ordinary project listener")
    else {
        panic!("fresh project unexpectedly dialed an existing supervisor")
    };
    assert!(serving.native_bootstrap_listener().is_none());
    assert!(!paths.native_bootstrap().exists());
}

#[test]
fn context_hash_is_byte_safe_and_preserves_argument_boundaries() {
    let separated = request_context(
        "atlas",
        vec![OsString::from_vec(vec![b'a', 0xff]), OsString::new()],
    );
    let joined = request_context("atlas", vec![OsString::from_vec(vec![b'a', 0xff, 0])]);
    let other_selector = request_context(
        "boreal",
        vec![OsString::from_vec(vec![b'a', 0xff]), OsString::new()],
    );

    assert_eq!(context_hash(&separated), context_hash(&separated));
    assert_ne!(context_hash(&separated), context_hash(&joined));
    assert_ne!(context_hash(&separated), context_hash(&other_selector));
}
