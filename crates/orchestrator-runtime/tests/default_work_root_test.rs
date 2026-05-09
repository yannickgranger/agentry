use orchestrator_runtime::daemon::default_work_root_inner;
use std::path::PathBuf;

#[test]
fn default_work_root_inner_uses_env_var_when_present() {
    let got = default_work_root_inner(
        Some("/custom/work/root".to_string()),
        Some("/home/anyone".to_string()),
    );
    assert_eq!(got, PathBuf::from("/custom/work/root"));
}

#[test]
fn default_work_root_inner_falls_back_to_xdg_share_under_home() {
    let got = default_work_root_inner(None, Some("/home/yg".to_string()));
    assert_eq!(got, PathBuf::from("/home/yg/.local/share/agentry/work"));
}

#[test]
fn default_work_root_inner_last_resort_when_neither_set() {
    let got = default_work_root_inner(None, None);
    assert_eq!(got, PathBuf::from("/tmp/agentry-work"));
}
