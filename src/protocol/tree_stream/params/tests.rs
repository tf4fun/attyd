use super::*;

fn parse(query: &str) -> Result<StreamQuery, ProtocolError> {
    parse_stream_query(query)
}

fn error(query: &str) -> String {
    let error = parse(query).expect_err(query);
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    error.message
}

#[test]
fn a_stream_follows_paths_with_what_the_client_holds() {
    let query = parse(
        "instance=I&follow=/sessions/s/turns&follow=/views/s/output/latest\
         &have=/sessions/s/turns:42&window=/views/s/output/latest:8192",
    )
    .unwrap();
    assert_eq!(query.instance.as_deref(), Some("I"));
    assert_eq!(
        query.follow,
        vec![
            Follow { path: "/sessions/s/turns".into(), have: Some(42), window: None },
            Follow { path: "/views/s/output/latest".into(), have: None, window: Some(8192) },
        ]
    );
}

#[test]
fn a_client_holding_nothing_needs_no_instance() {
    let query = parse("follow=/global").unwrap();
    assert_eq!(query.instance, None);
    assert_eq!(query.follow[0].have, None);
}

#[test]
fn a_have_follows_the_last_colon_of_a_path_that_holds_colons() {
    let query = parse("instance=I&follow=/sessions/a:b:c/op&have=/sessions/a:b:c/op:7").unwrap();
    assert_eq!(query.follow[0].have, Some(7));
}

#[test]
fn a_have_needs_the_instance_it_belongs_to() {
    assert!(error("follow=/global&have=/global:1").contains("instance"));
}

#[test]
fn paths_may_not_overlap() {
    for query in [
        "follow=/sessions/s&follow=/sessions/s/turns",
        "follow=/sessions/s/turns&follow=/sessions/s",
        "follow=/global&follow=/global",
    ] {
        assert!(error(query).contains("overlap"), "{query}");
    }
    // Siblings and a name that is only a string prefix of another do not overlap.
    parse("follow=/sessions/s/turns&follow=/sessions/s/turns2&follow=/sessions/s/op").unwrap();
}

#[test]
fn a_path_is_a_json_pointer_below_the_root() {
    for bad in ["follow=", "follow=/", "follow=global", "follow=/a~", "follow=/a~2b"] {
        assert!(error(bad).contains("not a path"), "{bad}");
    }
    parse("follow=/a~0b/c~1d").unwrap();
}

#[test]
fn what_names_a_path_not_followed_or_names_it_twice_is_rejected() {
    assert!(error("instance=I&follow=/global&have=/catalog:1").contains("not followed"));
    assert!(error("follow=/global&window=/catalog:8192").contains("not followed"));
    assert!(error("instance=I&follow=/global&have=/global:1&have=/global:2").contains("twice"));
    assert!(error("follow=/global&window=/global:8192&window=/global:8192").contains("twice"));
    assert!(error("instance=I&instance=J&follow=/global").contains("instance"));
}

#[test]
fn a_window_holds_at_least_a_segment() {
    assert!(error("follow=/global&window=/global:4095").contains("at least 4096"));
    parse("follow=/global&window=/global:4096").unwrap();
    assert!(error("follow=/global&window=/global:x").contains("no size"));
}

#[test]
fn a_stream_follows_something_and_knows_its_parameters() {
    assert!(error("").contains("at least one"));
    assert!(error("instance=I").contains("at least one"));
    assert!(error("follow=/global&scope=global").contains("unknown stream parameter"));
}

#[test]
fn a_cwd_names_a_followed_session_by_its_percent_encoded_id() {
    let query = parse("follow=/sessions/a%3Ab/op&cwd=a%253Ab:/work/x:y").unwrap();
    // The form decoding leaves `a%3Ab`, which is the ID `a:b`; the directory keeps its colon.
    assert_eq!(query.cwd.get("a:b").map(String::as_str), Some("/work/x:y"));
}

#[test]
fn a_cwd_of_a_view_or_a_session_path_holds_that_session() {
    let query = parse("follow=/views/s/output/latest&cwd=s:/w").unwrap();
    assert_eq!(query.cwd["s"], "/w");
}

#[test]
fn a_cwd_needs_a_followed_session_a_directory_and_once() {
    assert!(error("follow=/global&cwd=s:/w").contains("not followed"));
    assert!(error("follow=/sessions/s/op&cwd=s:").contains("no directory"));
    assert!(error("follow=/sessions/s/op&cwd=s").contains("<id>:<dir>"));
    assert!(error("follow=/sessions/s/op&cwd=s:/a&cwd=s:/b").contains("twice"));
    assert!(error("follow=/sessions/s/op&cwd=%25zz:/a").contains("percent-encoded"));
}

#[test]
fn the_session_of_a_path() {
    assert_eq!(session_of("/sessions/s/turns").as_deref(), Some("s"));
    assert_eq!(session_of("/views/a~1b/output/latest").as_deref(), Some("a/b"));
    assert_eq!(session_of("/sessions").as_deref(), None);
    assert_eq!(session_of("/global/sessions/s"), None);
    assert_eq!(session_of("/catalog"), None);
}
