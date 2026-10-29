use niri_config::{Action, Config, TagOperation};
use smithay::wayland::seat::WaylandFocus as _;
use wayland_client::protocol::wl_surface::WlSurface;

use super::client::ClientId;
use super::Fixture;
use crate::layout::LayoutElement as _;
use crate::window::Mapped;

fn set_up(config: &str, title: &str) -> (Fixture, ClientId, WlSurface) {
    let mut f = Fixture::with_config(Config::parse_mem(config).unwrap());
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.set_title(title);
    window.commit();
    f.roundtrip(id);
    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.set_size(100, 100);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    (f, id, surface)
}

fn mapped(f: &mut Fixture) -> &Mapped {
    f.niri()
        .layout
        .active_workspace()
        .unwrap()
        .active_window()
        .unwrap()
}

fn tags(f: &mut Fixture) -> Vec<String> {
    mapped(f).tags().cloned().collect()
}

#[test]
fn ordered_derivation_rebuilds_without_backward_matching_or_stale_cycles() {
    let (mut f, id, surface) = set_up(
        r#"
        window-rule { match tag="derived"; opacity 0.2; }
        window-rule { match title="^seed$"; tag "seed"; tag "seed"; }
        window-rule { match tag="seed"; tag "derived"; }
        window-rule { match tag="derived"; tag "seed"; scroll-factor 2.0; }
    "#,
        "seed",
    );
    assert_eq!(tags(&mut f), ["derived", "seed"]);
    assert_eq!(mapped(&mut f).rules().opacity, None);
    assert_eq!(mapped(&mut f).rules().scroll_factor, Some(2.0));

    // Re-evaluation must not let the previous result match an earlier rule.
    f.niri().recompute_window_rules();
    assert_eq!(mapped(&mut f).rules().opacity, None);

    f.client(id).window(&surface).set_title("ordinary");
    f.double_roundtrip(id);
    assert_eq!(tags(&mut f), Vec::<String>::new());
    assert_eq!(mapped(&mut f).rules().scroll_factor, None);
}

#[test]
fn manual_sources_survive_rule_changes_and_unset_does_not_remove_dynamic_source() {
    let (mut f, id, surface) = set_up(
        r#"
        window-rule { match title="^private$"; tag "private"; }
        window-rule { match tag="private"; opacity 0.5; }
    "#,
        "private",
    );
    let window_id = mapped(&mut f).id().get();
    f.niri_state().do_action(
        Action::TagById {
            id: window_id,
            operation: TagOperation::Set,
            tag: "private".into(),
        },
        false,
    );
    f.double_roundtrip(id);
    f.client(id).window(&surface).set_title("ordinary");
    f.double_roundtrip(id);
    assert_eq!(tags(&mut f), ["private"]);
    assert_eq!(mapped(&mut f).rules().opacity, Some(0.5));

    f.client(id).window(&surface).set_title("private");
    f.double_roundtrip(id);
    f.niri_state()
        .do_action(Action::Tag(TagOperation::Unset, "private".into()), false);
    f.double_roundtrip(id);
    assert_eq!(tags(&mut f), ["private"]);
    f.client(id).window(&surface).set_title("ordinary");
    f.double_roundtrip(id);
    assert_eq!(tags(&mut f), Vec::<String>::new());
    assert_eq!(mapped(&mut f).rules().opacity, None);

    f.niri_state()
        .do_action(Action::Tag(TagOperation::Toggle, "private".into()), false);
    f.double_roundtrip(id);
    // A config change must use the latest rule values, not a saved property snapshot.
    f.niri().config.borrow_mut().window_rules = Config::parse_mem(
        r#"
        window-rule { match tag="private"; opacity 0.7; }
    "#,
    )
    .unwrap()
    .window_rules;
    f.niri().recompute_window_rules();
    assert_eq!(tags(&mut f), ["private"]);
    assert_eq!(mapped(&mut f).rules().opacity, Some(0.7));
    f.niri_state().do_action(
        Action::TagById {
            id: window_id,
            operation: TagOperation::Toggle,
            tag: "private".into(),
        },
        false,
    );
    f.double_roundtrip(id);
    assert_eq!(mapped(&mut f).rules().opacity, None);
}

#[test]
fn remap_preserves_manual_tags_but_rebuilds_dynamic_tags() {
    let (mut f, id, surface) = set_up(
        r#"
        window-rule { match title="^seed$"; tag "dynamic"; }
        window-rule { match tag="manual"; opacity 0.6; }
    "#,
        "seed",
    );
    f.niri_state()
        .do_action(Action::Tag(TagOperation::Set, "manual".into()), false);
    f.double_roundtrip(id);
    assert_eq!(tags(&mut f), ["dynamic", "manual"]);
    let window = f.client(id).window(&surface);
    window.attach_null();
    window.commit();
    f.double_roundtrip(id);
    let window = f.client(id).window(&surface);
    window.set_title("ordinary");
    window.commit();
    f.double_roundtrip(id);
    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    assert_eq!(tags(&mut f), ["manual"]);
    assert_eq!(mapped(&mut f).rules().opacity, Some(0.6));
}

#[test]
fn tag_exclusion_and_exact_matching() {
    let (mut f, id, _) = set_up(
        r#"
        window-rule { exclude tag="allow"; tag "private"; }
        window-rule { match tag="private"; opacity 0.5; }
    "#,
        "ordinary",
    );
    f.niri_state()
        .do_action(Action::Tag(TagOperation::Set, "allow-more".into()), false);
    f.double_roundtrip(id);
    assert_eq!(mapped(&mut f).rules().opacity, Some(0.5));
    f.niri_state()
        .do_action(Action::Tag(TagOperation::Set, "allow".into()), false);
    f.double_roundtrip(id);
    assert_eq!(tags(&mut f), ["allow", "allow-more"]);
    assert_eq!(mapped(&mut f).rules().opacity, None);
}

#[test]
fn by_id_tag_actions_reach_interactively_moved_window() {
    let (mut f, id, _) = set_up(
        r#"window-rule { match tag="private"; opacity 0.5; }"#,
        "ordinary",
    );
    let output = f.niri_output(1);
    let window = mapped(&mut f).window.clone();
    let window_id = mapped(&mut f).id().get();
    assert!(f
        .niri()
        .layout
        .interactive_move_begin(window.clone(), &output, (0., 0.).into(),));
    f.niri()
        .layout
        .interactive_move_update(&window, (1000., 0.).into(), output, (0., 0.).into());
    f.double_roundtrip(id);
    assert!(!f
        .niri()
        .layout
        .workspaces_mut()
        .any(|ws| ws.windows_mut().any(|w| w.id().get() == window_id)));

    for (action, expected) in [
        (
            Action::TagById {
                id: window_id,
                operation: TagOperation::Set,
                tag: "private".into(),
            },
            Some(0.5),
        ),
        (
            Action::TagById {
                id: window_id,
                operation: TagOperation::Unset,
                tag: "private".into(),
            },
            None,
        ),
        (
            Action::TagById {
                id: window_id,
                operation: TagOperation::Toggle,
                tag: "private".into(),
            },
            Some(0.5),
        ),
        (
            Action::TagById {
                id: window_id,
                operation: TagOperation::Toggle,
                tag: "private".into(),
            },
            None,
        ),
    ] {
        f.niri_state().do_action(action, false);
        f.double_roundtrip(id);
        let mapped = f
            .niri()
            .layout
            .find_window_and_output(window.wl_surface().unwrap().as_ref())
            .unwrap()
            .0;
        assert_eq!(mapped.rules().opacity, expected);
        assert_eq!(
            mapped.tags().any(|tag| tag == "private"),
            expected.is_some()
        );
    }
}
