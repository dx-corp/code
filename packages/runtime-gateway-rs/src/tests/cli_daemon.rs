#[allow(unused_imports)]
use super::*;

#[test]
pub(crate) fn daemon_ownership_requires_one_valid_external_owner() {
    assert_eq!(parse_ownership_watch(None, None).unwrap(), None);
    assert_eq!(
        parse_ownership_watch(Some("42"), None).unwrap(),
        Some(OwnershipWatch::ParentPid(42))
    );
    assert_eq!(
        parse_ownership_watch(None, Some("7")).unwrap(),
        Some(OwnershipWatch::LivenessFd(7))
    );
    assert!(parse_ownership_watch(Some("42"), Some("7")).is_err());
    assert!(parse_ownership_watch(Some("0"), None).is_err());
    assert!(parse_ownership_watch(None, Some("-1")).is_err());
}

#[cfg(unix)]
#[tokio::test]
pub(crate) async fn daemon_liveness_pipe_detects_supervisor_disconnect() {
    let mut fds = [0_i32; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    assert_eq!(unsafe { libc::close(fds[1]) }, 0);
    let reason = tokio::time::timeout(
        Duration::from_secs(1),
        wait_for_owner_loss(OwnershipWatch::LivenessFd(fds[0])),
    )
    .await
    .expect("liveness monitor should observe EOF");
    assert_eq!(reason, format!("liveness_pipe_closed:{}", fds[0]));
}

#[test]
pub(crate) fn cli_args_default_to_serving() {
    assert_eq!(
        parse_cli_action(Vec::<String>::new()).unwrap(),
        CliAction::Serve
    );
}

#[test]
pub(crate) fn cli_args_handle_help_and_version_without_serving() {
    assert_eq!(parse_cli_action(["--help"]).unwrap(), CliAction::Help);
    assert_eq!(parse_cli_action(["-h"]).unwrap(), CliAction::Help);
    assert_eq!(parse_cli_action(["--version"]).unwrap(), CliAction::Version);
    assert_eq!(parse_cli_action(["-V"]).unwrap(), CliAction::Version);
}

#[test]
pub(crate) fn cli_args_reject_unknown_values() {
    let error = parse_cli_action(["--wat"]).unwrap_err();
    assert!(error.to_string().contains("--wat"));
}
