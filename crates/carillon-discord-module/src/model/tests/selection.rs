use super::*;

#[test]
fn provider_browsing_is_read_only_and_model_selection_uses_the_chosen_scope() -> Result<(), String>
{
    for scope in [Scope::Conversation, Scope::Agent] {
        let mut host = runtime(true);
        let name = super::super::scope_name(scope);
        let initial = expect_reply(interact(
            &mut host,
            &component(&format!("model:scope:{name}"), &[]),
        ))?;
        assert_eq!(
            selected(&initial, &provider_control(name, "anthropic")),
            Some(fingerprint(&["anthropic"]))
        );
        assert_eq!(
            values(&initial, &format!("model:model:{name}")),
            [key("anthropic", "opus"), key("anthropic", "sonnet")]
        );
        let before = host.state.clone();
        let calls = host.requests.len();
        let browsed = expect_reply(interact(
            &mut host,
            &component(
                &provider_control(name, "anthropic"),
                &[&fingerprint(&["openai"])],
            ),
        ))?;
        assert_eq!(host.state, before);
        assert_eq!(&host.requests[calls..], &[HostRequest::ModelState]);
        assert!(browsed.content.contains("This conversation: sonnet name"));
        assert!(
            browsed
                .content
                .contains("Default for new conversations: sonnet name")
        );
        assert!(browsed.content.contains("Browsing provider: openai"));
        assert_eq!(
            values(&browsed, &format!("model:model:{name}")),
            [key("openai", "gpt")]
        );
        assert_eq!(selected(&browsed, &format!("model:model:{name}")), None);
        assert!(
            !custom_ids(&browsed)
                .iter()
                .any(|id| id.starts_with("model:option:"))
        );
        let applied = expect_reply(interact(
            &mut host,
            &component(&format!("model:model:{name}"), &[&key("openai", "gpt")]),
        ))?;
        assert_eq!(
            host.requests.last(),
            Some(&HostRequest::SetModel {
                scope,
                selection: Some(selection("openai", "gpt", None)),
                expected: None,
            })
        );
        assert_eq!(
            selected(&applied, &provider_control(name, "openai")),
            Some(fingerprint(&["openai"]))
        );
        assert_eq!(
            selected(&applied, &format!("model:model:{name}")),
            Some(key("openai", "gpt"))
        );
        let other_scope = if scope == Scope::Agent {
            Scope::Conversation
        } else {
            Scope::Agent
        };
        assert_eq!(
            super::super::effective(&host.state, other_scope),
            super::super::effective(&before, other_scope)
        );
        let other = expect_reply(interact(
            &mut host,
            &component(
                &format!("model:scope:{}", super::super::scope_name(other_scope)),
                &[],
            ),
        ))?;
        assert!(other.content.contains("Browsing provider: anthropic"));
    }
    Ok(())
}

#[test]
fn provider_removal_and_paging_do_not_retarget_a_selection() -> Result<(), String> {
    let mut host = runtime(true);
    for index in 0..60 {
        host.state
            .available
            .push(model("zeta", &format!("model-{index:02}"), &[]));
    }
    let before = host.state.clone();
    let browsed = expect_reply(interact(
        &mut host,
        &component(
            &provider_control("conversation", "anthropic"),
            &[&fingerprint(&["zeta"])],
        ),
    ))?;
    assert!(browsed.content.contains("Browsing provider: zeta"));
    let page = expect_reply(interact(
        &mut host,
        &component(&model_page("conversation", "zeta", 1), &[]),
    ))?;
    assert!(page.content.contains("Browsing provider: zeta"));
    assert_eq!(values(&page, "model:model:conversation").len(), PAGE);
    assert!(values(&page, "model:model:conversation").contains(&key("zeta", "model-25")));
    assert_eq!(host.state, before);
    host.state
        .available
        .retain(|model| model.provider != "zeta");
    for control in [
        component(&model_page("conversation", "zeta", 2), &[]),
        component(
            &provider_control("conversation", "anthropic"),
            &[&fingerprint(&["zeta"])],
        ),
    ] {
        let reply = expect_reply(interact(&mut host, &control))?;
        assert!(
            reply
                .content
                .contains("That provider is no longer available")
        );
        assert!(reply.content.contains("Browsing provider: anthropic"));
    }
    assert!(
        host.requests
            .iter()
            .all(|request| *request == HostRequest::ModelState)
    );
    assert_eq!(host.state.conversation, before.conversation);
    Ok(())
}

#[test]
fn provider_overflow_keeps_every_provider_reachable_within_the_row_limit() -> Result<(), String> {
    let mut host = runtime(true);
    for index in 0..60 {
        host.state
            .available
            .push(model(&format!("provider-{index:02}"), "model", &[]));
    }
    for index in 0..30 {
        host.state
            .available
            .push(model("anthropic", &format!("model-{index:02}"), &[]));
    }
    host.state.available[0].options = (0..4)
        .map(|index| ModelOption {
            id: format!("setting-{index}"),
            name: format!("Setting {index}"),
            choices: choices(&["one", "two"]),
            default: "one".to_owned(),
        })
        .collect();
    let before = host.state.clone();
    let expected: std::collections::BTreeSet<_> = host
        .state
        .available
        .iter()
        .map(|model| fingerprint(&[&model.provider]))
        .collect();
    let mut seen = std::collections::BTreeSet::new();
    let control = provider_control("conversation", "anthropic");
    let mut reply = expect_reply(interact(&mut host, &command(true)))?;
    loop {
        assert_eq!(reply.components.len(), 5);
        let choices = values(&reply, &control);
        let previously_seen = seen.len();
        seen.extend(
            choices
                .iter()
                .filter(|value| !value.starts_with("page:"))
                .cloned(),
        );
        assert!(
            seen.len() > previously_seen,
            "provider paging must make progress"
        );
        let next = component_by_id(&reply, &control)["options"]
            .as_array()
            .ok_or("provider choices")?
            .iter()
            .find(|option| option["label"] == "Next providers ▶")
            .and_then(|option| option["value"].as_str())
            .map(str::to_owned);
        let Some(next) = next else { break };
        reply = expect_reply(interact(&mut host, &component(&control, &[&next])))?;
    }
    assert_eq!(seen, expected);
    assert_eq!(host.state, before);
    assert!(
        host.requests
            .iter()
            .all(|request| *request == HostRequest::ModelState)
    );
    let chosen = "provider-59";
    let reply = expect_reply(interact(
        &mut host,
        &component(&control, &[&fingerprint(&[chosen])]),
    ))?;
    assert_eq!(
        selected(&reply, &provider_control("conversation", chosen)),
        Some(fingerprint(&[chosen]))
    );
    assert_eq!(
        values(&reply, "model:model:conversation"),
        [key(chosen, "model")]
    );
    Ok(())
}

#[test]
fn scope_switching_and_defaults_apply_to_the_displayed_scope() -> Result<(), String> {
    let mut host = runtime(true);
    let reply = expect_reply(interact(&mut host, &component("model:scope:agent", &[])))?;
    assert!(custom_ids(&reply).contains(&"model:default:agent".to_owned()));
    assert_eq!(component_by_id(&reply, "model:scope:agent")["style"], 1);
    let reply = expect_reply(interact(
        &mut host,
        &component("model:model:agent", &[&key("anthropic", "opus")]),
    ))?;
    assert!(
        reply
            .content
            .contains("Default for new conversations: opus name")
    );
    assert!(reply.content.contains("(set with /model)"));
    let reply = expect_reply(interact(&mut host, &component("model:default:agent", &[])))?;
    assert_eq!(
        host.requests[3],
        HostRequest::SetModel {
            scope: Scope::Agent,
            selection: None,
            expected: None,
        }
    );
    assert!(reply.content.contains("(runtime default)"));
    Ok(())
}

#[test]
fn paging_reaches_every_model_and_opens_on_the_selection() -> Result<(), String> {
    let mut host = runtime(true);
    for index in 0..60 {
        host.state
            .available
            .push(model("zeta", &format!("model-{index:02}"), &[]));
    }
    let last = selection("zeta", "model-59", None);
    host.state.conversation = Some(ScopeState {
        selection: Some(last.clone()),
        overridden: true,
        read_only: false,
    });
    let reply = expect_reply(interact(&mut host, &command(true)))?;
    let model = component_by_id(&reply, "model:model:conversation");
    assert_eq!(model["placeholder"], "Model — showing 51–60 of 60");
    assert_eq!(
        selected(&reply, "model:model:conversation"),
        Some(key("zeta", "model-59"))
    );
    assert_eq!(
        component_by_id(&reply, &model_page("conversation", "zeta", 1))["disabled"],
        false
    );
    assert_eq!(
        component_by_id(&reply, &model_page("conversation", "zeta", 3))["disabled"],
        true
    );

    let mut seen = Vec::new();
    for page in 0..3 {
        let reply = expect_reply(interact(
            &mut host,
            &component(&model_page("conversation", "zeta", page), &[]),
        ))?;
        seen.extend(values(&reply, "model:model:conversation"));
    }
    seen.sort_unstable();
    let mut expected: Vec<String> = host
        .state
        .available
        .iter()
        .filter(|model| model.provider == "zeta")
        .map(super::super::model_key)
        .collect();
    expected.sort_unstable();
    assert_eq!(seen, expected);
    assert_eq!(host.requests.len(), 4);

    // Choosing from another page applies and returns to the selection's page.
    let reply = expect_reply(interact(
        &mut host,
        &component("model:model:conversation", &[&key("zeta", "model-00")]),
    ))?;
    assert_eq!(
        host.requests[5],
        HostRequest::SetModel {
            scope: Scope::Conversation,
            selection: Some(selection("zeta", "model-00", None)),
            expected: None,
        }
    );
    assert_eq!(
        component_by_id(&reply, "model:model:conversation")["placeholder"],
        "Model — showing 1–25 of 60"
    );
    Ok(())
}

#[test]
fn identifiers_of_any_length_round_trip_unchanged() -> Result<(), String> {
    let mut host = runtime(true);
    let long = "m".repeat(150);
    let provider = "p:".repeat(90);
    host.state
        .available
        .push(model(&provider, &long, &["off", "on"]));
    let reply = expect_reply(interact(
        &mut host,
        &component(
            &provider_control("conversation", "anthropic"),
            &[&fingerprint(&[&provider])],
        ),
    ))?;
    let model = component_by_id(&reply, "model:model:conversation");
    let option = model["options"]
        .as_array()
        .and_then(|options| {
            options
                .iter()
                .find(|option| option["value"] == key(&provider, &long))
        })
        .ok_or("the long model is offered")?;
    assert_eq!(option["label"].as_str().map(str::len), Some(TEXT_LIMIT));
    assert_eq!(
        option["description"].as_str().map(str::len),
        Some(TEXT_LIMIT)
    );
    interact(
        &mut host,
        &component("model:model:conversation", &[&key(&provider, &long)]),
    );
    assert_eq!(
        host.requests[2],
        HostRequest::SetModel {
            scope: Scope::Conversation,
            selection: Some(selection(&provider, &long, None)),
            expected: None,
        }
    );
    Ok(())
}

#[test]
fn a_setting_from_an_old_panel_never_targets_a_different_model() -> Result<(), String> {
    for scope in [Scope::Agent, Scope::Conversation] {
        let mut host = runtime(true);
        let scope_name = if scope == Scope::Agent {
            "agent"
        } else {
            "conversation"
        };
        let old = expect_reply(interact(
            &mut host,
            &component(&format!("model:scope:{scope_name}"), &[]),
        ))?;
        let id = custom_ids(&old)
            .into_iter()
            .find(|id| id.starts_with("model:option:"))
            .ok_or("thinking control")?;
        let high = component_by_id(&old, &id)["options"]
            .as_array()
            .ok_or("choices")?
            .iter()
            .find(|option| option["label"] == "high")
            .and_then(|option| option["value"].as_str())
            .ok_or("high choice")?;
        // Both models offer the exact same setting/value. Identity, not
        // catalogue membership, must prevent this old control from writing.
        let target = if scope == Scope::Agent {
            &mut host.state.agent
        } else {
            host.state.conversation.as_mut().ok_or("conversation")?
        };
        target.selection = Some(selection("anthropic", "opus", Some("off")));
        let before = host.state.clone();
        let calls = host.requests.len();
        let reply = expect_reply(interact(&mut host, &component(&id, &[high])))?;
        assert!(
            reply
                .content
                .contains("That setting is no longer available")
        );
        assert!(reply.content.contains("opus name"));
        assert_eq!(host.state, before);
        assert_eq!(&host.requests[calls..], &[HostRequest::ModelState]);
    }
    Ok(())
}

#[test]
fn stale_controls_are_reported_without_changing_anything() -> Result<(), String> {
    let mut host = runtime(true);
    let reply = expect_reply(interact(
        &mut host,
        &component("model:model:conversation", &[&key("anthropic", "retired")]),
    ))?;
    assert!(reply.content.contains("That model is no longer available"));
    assert!(custom_ids(&reply).contains(&"model:model:conversation".to_owned()));
    let reply = expect_reply(interact(
        &mut host,
        &component(&thinking_control("conversation"), &[&fingerprint(&["max"])]),
    ))?;
    assert!(
        reply
            .content
            .contains("That setting is no longer available")
    );
    let reply = expect_reply(interact(
        &mut host,
        &component(
            &option_control("conversation", "anthropic", "sonnet", "speed"),
            &[&fingerprint(&["fast"])],
        ),
    ))?;
    assert!(
        reply
            .content
            .contains("That setting is no longer available")
    );
    assert_eq!(host.requests, [const { HostRequest::ModelState }; 3]);
    Ok(())
}

#[test]
fn a_selection_missing_from_the_catalogue_leaves_no_empty_control() -> Result<(), String> {
    let mut host = runtime(true);
    host.state.conversation = Some(ScopeState {
        selection: Some(selection("removed", "gone", Some("high"))),
        overridden: true,
        read_only: false,
    });
    let reply = expect_reply(interact(&mut host, &command(true)))?;
    assert!(
        reply
            .content
            .contains("`removed/gone` (not currently available)")
    );
    assert_eq!(
        custom_ids(&reply),
        [
            provider_control("conversation", "anthropic"),
            "model:model:conversation".to_owned(),
            "model:scope:conversation".to_owned(),
            "model:scope:agent".to_owned(),
            "model:default:conversation".to_owned(),
        ]
    );
    assert_eq!(selected(&reply, "model:model:conversation"), None);
    assert_eq!(values(&reply, "model:model:conversation").len(), 2);
    assert_eq!(
        component_by_id(&reply, "model:default:conversation")["disabled"],
        false
    );
    Ok(())
}

#[test]
fn many_settings_are_reached_through_a_setting_chooser() -> Result<(), String> {
    let mut host = runtime(true);
    let options = ["thinking", "speed", "verbosity", "format"];
    host.state.available[0].options = options
        .iter()
        .map(|id| ModelOption {
            id: (*id).to_owned(),
            name: id.to_uppercase(),
            choices: choices(&["one", "two"]),
            default: "one".to_owned(),
        })
        .collect();
    let shown: BTreeMap<String, String> = options
        .iter()
        .map(|id| ((*id).to_owned(), "one".to_owned()))
        .collect();
    host.state.conversation = Some(ScopeState {
        selection: Some(Selection {
            options: shown.clone(),
            ..selection("anthropic", "sonnet", None)
        }),
        overridden: false,
        read_only: false,
    });
    let reply = expect_reply(interact(&mut host, &command(true)))?;
    assert_eq!(
        custom_ids(&reply)[..4],
        [
            provider_control("conversation", "anthropic"),
            "model:model:conversation".to_owned(),
            "model:setting:conversation".to_owned(),
            thinking_control("conversation"),
        ]
    );
    assert_eq!(
        values(&reply, "model:setting:conversation"),
        options
            .iter()
            .map(|id| fingerprint(&[id]))
            .collect::<Vec<_>>()
    );
    let speed = option_control("conversation", "anthropic", "sonnet", "speed");
    let reply = expect_reply(interact(
        &mut host,
        &component("model:setting:conversation", &[&fingerprint(&["speed"])]),
    ))?;
    assert!(custom_ids(&reply).contains(&speed));
    assert_eq!(
        selected(&reply, "model:setting:conversation"),
        Some(fingerprint(&["speed"]))
    );
    // Changing one setting records every displayed setting, so the scope
    // runs exactly what the panel showed.
    let reply = expect_reply(interact(
        &mut host,
        &component(&speed, &[&fingerprint(&["two"])]),
    ))?;
    let expected = Some(Selection {
        options: shown.clone(),
        ..selection("anthropic", "sonnet", None)
    });
    let mut pinned = shown;
    pinned.insert("speed".to_owned(), "two".to_owned());
    assert_eq!(
        host.requests[3],
        HostRequest::SetModel {
            scope: Scope::Conversation,
            selection: Some(Selection {
                options: pinned,
                ..selection("anthropic", "sonnet", None)
            }),
            expected,
        }
    );
    assert_eq!(selected(&reply, &speed), Some(fingerprint(&["two"])));
    Ok(())
}

#[test]
fn a_refused_change_shows_the_current_state_without_retrying() -> Result<(), String> {
    let mut host = runtime(true);
    host.refusal = Some("The model settings changed.".to_owned());
    host.concurrent = Some(selection("anthropic", "opus", Some("high")));
    let high = fingerprint(&["high"]);
    let reply = expect_reply(interact(
        &mut host,
        &component(&thinking_control("conversation"), &[&high]),
    ))?;
    assert!(
        reply.content.contains(
            "Your change was not confirmed: The model settings changed. The settings below are current."
        ),
        "{}",
        reply.content
    );
    // The panel is rebuilt from the state read after the refusal, so it
    // shows the other writer's change and controls bound to it.
    assert!(reply.content.contains("This conversation: opus name"));
    assert!(custom_ids(&reply).contains(&option_control(
        "conversation",
        "anthropic",
        "opus",
        "thinking"
    )));
    assert_eq!(
        host.requests
            .iter()
            .filter(|request| matches!(request, HostRequest::SetModel { .. }))
            .count(),
        1,
        "a refused change is not retried"
    );
    assert_eq!(host.requests.last(), Some(&HostRequest::ModelState));

    for control in [
        component("model:default:agent", &[]),
        component("model:model:agent", &[&key("openai", "gpt")]),
    ] {
        let reply = expect_reply(interact(&mut host, &control))?;
        assert!(reply.content.contains("Your change was not confirmed"));
        assert!(
            reply
                .content
                .contains("Changing the default for new conversations:")
        );
        assert!(custom_ids(&reply).contains(&"model:default:agent".to_owned()));
    }
    Ok(())
}

#[test]
fn a_refused_change_without_current_state_says_both() -> Result<(), String> {
    let mut host = runtime(true);
    let long = "🦀".repeat(3000);
    host.refusal = Some(long.clone());
    host.then_fail = Some(long);
    let reply = expect_reply(interact(&mut host, &component("model:default:agent", &[])))?;
    assert!(reply.content.starts_with("Your change was not confirmed: "));
    assert!(reply.content.contains("Model settings are unavailable: "));
    assert!(reply.content.ends_with("Reopen /model to refresh."));
    assert!(reply.components.is_empty());
    Ok(())
}

#[test]
fn unavailable_settings_and_unknown_controls_are_reported() -> Result<(), String> {
    let mut host = runtime(true);
    host.failure = Some("runtime provider is unavailable".to_owned());
    let reply = expect_reply(interact(&mut host, &command(true)))?;
    assert_eq!(
        reply.content,
        "Model settings are unavailable: runtime provider is unavailable Reopen /model to refresh."
    );
    assert!(reply.components.is_empty());

    let mut host = runtime(true);
    for (custom_id, values) in [
        ("model:bogus:conversation", &["x"][..]),
        ("other:default:agent", &[]),
        ("model:page:conversation:many", &[]),
        ("model:model:conversation", &[]),
    ] {
        assert!(matches!(
            interact(&mut host, &component(custom_id, values)),
            Outcome::Failure(_)
        ));
    }
    Ok(())
}
