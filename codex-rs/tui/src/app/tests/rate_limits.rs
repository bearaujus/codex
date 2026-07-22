use super::*;
use codex_app_server_protocol::AccountRateLimitsUpdatedNotification;
use codex_app_server_protocol::CodexErrorInfo;
use codex_app_server_protocol::CreditsSnapshot;
use codex_app_server_protocol::ErrorNotification;
use codex_app_server_protocol::GetAccountRateLimitsResponse;
use codex_app_server_protocol::RateLimitReachedType;
use codex_app_server_protocol::RateLimitResetCreditsSummary;
use codex_app_server_protocol::RateLimitSnapshot;
use codex_app_server_protocol::RateLimitWindow;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use pretty_assertions::assert_eq;

fn rate_limit_snapshot(
    used_percent: i32,
    rate_limit_reached_type: Option<RateLimitReachedType>,
    spend_control_reached: Option<bool>,
) -> RateLimitSnapshot {
    RateLimitSnapshot {
        limit_id: Some("codex".to_string()),
        limit_name: None,
        primary: Some(RateLimitWindow {
            used_percent,
            window_duration_mins: Some(300),
            resets_at: None,
        }),
        secondary: None,
        credits: Some(CreditsSnapshot {
            has_credits: true,
            unlimited: false,
            balance: None,
        }),
        individual_limit: None,
        spend_control_reached,
        plan_type: None,
        rate_limit_reached_type,
    }
}

fn account_rate_limits_response(snapshot: RateLimitSnapshot) -> GetAccountRateLimitsResponse {
    GetAccountRateLimitsResponse {
        rate_limits: snapshot,
        rate_limits_by_limit_id: None,
        rate_limit_reset_credits: Some(RateLimitResetCreditsSummary {
            available_count: 0,
            credits: None,
        }),
    }
}

async fn deliver_rolling_rate_limit_snapshot(
    app: &mut App,
    app_server: &AppServerSession,
    snapshot: RateLimitSnapshot,
) {
    deliver_rolling_rate_limit_update(app, app_server, Some(snapshot)).await;
}

async fn deliver_rolling_rate_limit_update(
    app: &mut App,
    app_server: &AppServerSession,
    rate_limits: Option<RateLimitSnapshot>,
) {
    app.handle_app_server_event(
        app_server,
        codex_app_server_client::AppServerEvent::ServerNotification(
            ServerNotification::AccountRateLimitsUpdated(AccountRateLimitsUpdatedNotification {
                rate_limits,
            }),
        ),
    )
    .await;
}

#[tokio::test]
async fn rolling_rate_limit_clear_removes_previous_account_status() -> Result<()> {
    let (mut app, mut app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let app_server = crate::start_embedded_app_server_for_picker(app.chat_widget.config_ref())
        .await
        .expect("embedded app server");

    deliver_rolling_rate_limit_snapshot(
        &mut app,
        &app_server,
        rate_limit_snapshot(
            /*used_percent*/ 95,
            Some(RateLimitReachedType::WorkspaceMemberUsageLimitReached),
            Some(true),
        ),
    )
    .await;
    let before_clear = render_status_output(&mut app, &mut app_event_rx);
    assert!(
        before_clear.contains("5% left"),
        "unexpected status: {before_clear}"
    );

    deliver_rolling_rate_limit_update(&mut app, &app_server, /*rate_limits*/ None).await;

    let after_clear = render_status_output(&mut app, &mut app_event_rx);
    assert!(
        !after_clear.contains("5% left"),
        "cleared limits remained visible: {after_clear}"
    );
    insta::with_settings!({snapshot_path => "../snapshots"}, {
        insta::assert_snapshot!(
            "rolling_rate_limit_clear_removes_previous_account_status",
            format!("Before clear:\n{before_clear}\n\nAfter clear:\n{after_clear}")
        );
    });
    app_server.shutdown().await?;
    Ok(())
}

fn render_status_output(
    app: &mut App,
    app_event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
) -> String {
    while app_event_rx.try_recv().is_ok() {}
    app.chat_widget.add_status_output(
        /*refreshing_rate_limits*/ false, /*request_id*/ None,
    );
    match app_event_rx.try_recv() {
        Ok(AppEvent::InsertHistoryCell(cell)) => cell
            .display_lines(/*width*/ 120)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        other => panic!("expected status output, got {other:?}"),
    }
}

fn deliver_usage_limit_error(app: &mut App) {
    app.chat_widget.handle_server_notification(
        ServerNotification::Error(ErrorNotification {
            error: AppServerTurnError {
                message: "Usage limit reached.".to_string(),
                codex_error_info: Some(CodexErrorInfo::UsageLimitExceeded),
                additional_details: None,
            },
            will_retry: false,
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
        }),
        /*replay_kind*/ None,
    );
}

#[tokio::test]
async fn every_rolling_rate_limit_update_invalidates_older_reads() -> Result<()> {
    let (mut app, _app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let app_server = crate::start_embedded_app_server_for_picker(app.chat_widget.config_ref())
        .await
        .expect("embedded app server");

    let cases = [
        (None, None),
        (Some(RateLimitReachedType::RateLimitReached), None),
        (None, Some(false)),
        (None, Some(true)),
        (
            Some(RateLimitReachedType::WorkspaceOwnerCreditsDepleted),
            None,
        ),
        (
            Some(RateLimitReachedType::WorkspaceMemberCreditsDepleted),
            None,
        ),
        (
            Some(RateLimitReachedType::WorkspaceOwnerUsageLimitReached),
            None,
        ),
        (
            Some(RateLimitReachedType::WorkspaceMemberUsageLimitReached),
            None,
        ),
    ];
    let mut expected_generation = 0;
    for (reached_type, spend_control_reached) in cases {
        deliver_rolling_rate_limit_snapshot(
            &mut app,
            &app_server,
            rate_limit_snapshot(
                /*used_percent*/ 95,
                reached_type,
                spend_control_reached,
            ),
        )
        .await;
        expected_generation += 1;
        assert_eq!(
            app.rate_limit_update_generation, expected_generation,
            "reached_type={reached_type:?}, spend_control_reached={spend_control_reached:?}"
        );
    }

    deliver_rolling_rate_limit_update(&mut app, &app_server, /*rate_limits*/ None).await;
    assert_eq!(app.rate_limit_update_generation, expected_generation + 1);

    app_server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn stale_rate_limit_reads_preserve_newer_workspace_hard_stop_for_every_origin() -> Result<()>
{
    for origin_name in [
        "startup",
        "status",
        "usage",
        "reset-picker",
        "reset-consume",
    ] {
        let (mut app, mut app_event_rx, _op_rx) = make_test_app_with_channels().await;
        set_chatgpt_auth(&mut app.chat_widget);
        let mut tui = crate::tui::test_support::make_test_tui()?;
        let mut app_server = Box::pin(crate::start_embedded_app_server_for_picker(
            app.chat_widget.config_ref(),
        ))
        .await?;

        let origin = match origin_name {
            "startup" => RateLimitRefreshOrigin::StartupPrefetch {
                reset_hint_request_id: app.chat_widget.start_rate_limit_reset_startup_check(),
            },
            "status" => {
                let request_id = 7;
                app.chat_widget
                    .add_status_output(/*refreshing_rate_limits*/ true, Some(request_id));
                RateLimitRefreshOrigin::StatusCommand { request_id }
            }
            "usage" => {
                let startup_request_id = app.chat_widget.start_rate_limit_reset_startup_check();
                app.chat_widget.finish_rate_limit_reset_hint_refresh(
                    startup_request_id,
                    Vec::new(),
                    Ok(RateLimitResetCreditsSummary {
                        available_count: 0,
                        credits: None,
                    }),
                );
                app.chat_widget.insert_str("/usage");
                app.chat_widget
                    .handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
                app.chat_widget
                    .handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                loop {
                    match app_event_rx.try_recv() {
                        Ok(AppEvent::RefreshRateLimits { origin }) => break origin,
                        Ok(_) => {}
                        other => panic!("expected usage refresh request, got {other:?}"),
                    }
                }
            }
            "reset-picker" => RateLimitRefreshOrigin::ResetPicker {
                request_id: app.chat_widget.show_rate_limit_reset_loading_popup(),
            },
            "reset-consume" => RateLimitRefreshOrigin::ResetConsume {
                request_id: app.chat_widget.show_rate_limit_reset_consuming_popup(),
            },
            _ => unreachable!("unknown refresh origin"),
        };
        let read_generation = app.rate_limit_update_generation;
        let mut rolling_snapshot = rate_limit_snapshot(
            /*used_percent*/ 95,
            Some(RateLimitReachedType::WorkspaceMemberUsageLimitReached),
            Some(true),
        );
        if origin_name == "reset-picker" {
            rolling_snapshot.limit_id = Some("codex_other".to_string());
        }
        deliver_rolling_rate_limit_snapshot(&mut app, &app_server, rolling_snapshot).await;
        assert_ne!(read_generation, app.rate_limit_update_generation);

        let control = Box::pin(app.handle_event(
            &mut tui,
            &mut app_server,
            AppEvent::RateLimitsLoaded {
                origin,
                update_generation: read_generation,
                result: Ok(account_rate_limits_response(rate_limit_snapshot(
                    /*used_percent*/ 0,
                    /*rate_limit_reached_type*/ None,
                    Some(false),
                ))),
            },
        ))
        .await?;
        assert!(matches!(control, AppRunControl::Continue));

        let popup = render_bottom_popup(&app.chat_widget, /*width*/ 100);
        match origin_name {
            "usage" => assert!(popup.contains("No usage limit resets available.")),
            "reset-picker" => {
                assert!(popup.contains("You don't have any usage limit resets available."));
            }
            "reset-consume" => {
                assert!(popup.contains("Usage reset. You have 0 usage limit resets left."));
            }
            "startup" | "status" => {}
            _ => unreachable!("unknown refresh origin"),
        }

        let status = render_status_output(&mut app, &mut app_event_rx);
        assert!(
            status.contains("5% left"),
            "expected {origin_name} to preserve rolling limits, got: {status}"
        );
        deliver_usage_limit_error(&mut app);
        let popup = render_bottom_popup(&app.chat_widget, /*width*/ 100);
        assert!(
            popup.contains("Request a limit increase from your owner"),
            "expected {origin_name} to preserve workspace error routing, got: {popup}"
        );

        app_server.shutdown().await?;
    }

    Ok(())
}

#[tokio::test]
async fn stale_rate_limit_read_preserves_newer_ordinary_usage_update() -> Result<()> {
    let (mut app, mut app_event_rx, _op_rx) = make_test_app_with_channels().await;
    set_chatgpt_auth(&mut app.chat_widget);
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let mut app_server = Box::pin(crate::start_embedded_app_server_for_picker(
        app.chat_widget.config_ref(),
    ))
    .await?;
    let request_id = 7;
    app.chat_widget
        .add_status_output(/*refreshing_rate_limits*/ true, Some(request_id));
    let read_generation = app.rate_limit_update_generation;

    deliver_rolling_rate_limit_snapshot(
        &mut app,
        &app_server,
        rate_limit_snapshot(
            /*used_percent*/ 80,
            /*rate_limit_reached_type*/ None,
            Some(false),
        ),
    )
    .await;

    Box::pin(app.handle_event(
        &mut tui,
        &mut app_server,
        AppEvent::RateLimitsLoaded {
            origin: RateLimitRefreshOrigin::StatusCommand { request_id },
            update_generation: read_generation,
            result: Ok(account_rate_limits_response(rate_limit_snapshot(
                /*used_percent*/ 10,
                /*rate_limit_reached_type*/ None,
                Some(false),
            ))),
        },
    ))
    .await?;

    let status = render_status_output(&mut app, &mut app_event_rx);
    assert!(
        status.contains("20% left"),
        "expected newer rolling usage to win, got: {status}"
    );

    app_server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn stale_rate_limit_read_does_not_dismiss_visible_workspace_advisory() -> Result<()> {
    let (mut app, _app_event_rx, _op_rx) = make_test_app_with_channels().await;
    set_chatgpt_auth(&mut app.chat_widget);
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let mut app_server = Box::pin(crate::start_embedded_app_server_for_picker(
        app.chat_widget.config_ref(),
    ))
    .await?;
    let request_id = 7;
    app.chat_widget
        .add_status_output(/*refreshing_rate_limits*/ true, Some(request_id));
    let read_generation = app.rate_limit_update_generation;

    deliver_rolling_rate_limit_snapshot(
        &mut app,
        &app_server,
        rate_limit_snapshot(
            /*used_percent*/ 95,
            Some(RateLimitReachedType::WorkspaceMemberUsageLimitReached),
            Some(true),
        ),
    )
    .await;
    app.chat_widget.handle_server_notification(
        turn_completed_notification(ThreadId::new(), "turn-1", TurnStatus::Completed),
        /*replay_kind*/ None,
    );
    assert!(
        render_bottom_popup(&app.chat_widget, /*width*/ 100).contains("Approaching rate limits")
    );

    Box::pin(app.handle_event(
        &mut tui,
        &mut app_server,
        AppEvent::RateLimitsLoaded {
            origin: RateLimitRefreshOrigin::StatusCommand { request_id },
            update_generation: read_generation,
            result: Ok(account_rate_limits_response(rate_limit_snapshot(
                /*used_percent*/ 0,
                /*rate_limit_reached_type*/ None,
                Some(false),
            ))),
        },
    ))
    .await?;

    assert!(
        render_bottom_popup(&app.chat_widget, /*width*/ 100).contains("Approaching rate limits")
    );
    app_server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn post_hard_stop_rate_limit_read_clears_recovered_workspace_limit() -> Result<()> {
    let (mut app, mut app_event_rx, _op_rx) = make_test_app_with_channels().await;
    set_chatgpt_auth(&mut app.chat_widget);
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let mut app_server = Box::pin(crate::start_embedded_app_server_for_picker(
        app.chat_widget.config_ref(),
    ))
    .await?;
    deliver_rolling_rate_limit_snapshot(
        &mut app,
        &app_server,
        rate_limit_snapshot(
            /*used_percent*/ 95,
            Some(RateLimitReachedType::WorkspaceMemberUsageLimitReached),
            Some(true),
        ),
    )
    .await;
    let read_generation = app.rate_limit_update_generation;
    let request_id = 7;
    app.chat_widget
        .add_status_output(/*refreshing_rate_limits*/ true, Some(request_id));

    let control = Box::pin(app.handle_event(
        &mut tui,
        &mut app_server,
        AppEvent::RateLimitsLoaded {
            origin: RateLimitRefreshOrigin::StatusCommand { request_id },
            update_generation: read_generation,
            result: Ok(account_rate_limits_response(rate_limit_snapshot(
                /*used_percent*/ 0,
                /*rate_limit_reached_type*/ None,
                Some(false),
            ))),
        },
    ))
    .await?;
    assert!(matches!(control, AppRunControl::Continue));

    let status = render_status_output(&mut app, &mut app_event_rx);
    assert!(
        status.contains("100% left"),
        "expected recovered limits, got: {status}"
    );
    deliver_usage_limit_error(&mut app);
    let popup = render_bottom_popup(&app.chat_widget, /*width*/ 100);
    assert!(
        !popup.contains("Request a limit increase from your owner"),
        "expected recovered state to clear workspace error routing, got: {popup}"
    );

    app_server.shutdown().await?;
    Ok(())
}
