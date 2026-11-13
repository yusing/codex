use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paused_usage_limit_steer_reaches_next_request_in_original_turn() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/responses"))
        .respond_with(wiremock::ResponseTemplate::new(429).set_body_json(json!({
            "error": {"type": "usage_limit_reached"}
        })))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    let response = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("recovered"),
            core_test_support::responses::ev_assistant_message("answer", "explicit input accepted"),
            core_test_support::responses::ev_completed("recovered"),
        ]),
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config
                .features
                .enable(codex_features::Feature::MultiAgentV2)
                .unwrap();
        })
        .build_with_auto_env(&server)
        .await?;
    let input = |text: &str| {
        TurnInputRequest::user_input(vec![UserInput::Text {
            text: text.to_string(),
            text_elements: Vec::new(),
        }])
    };
    let codex_core::TurnInputSubmission::Started { turn_id, .. } = test
        .codex
        .start_or_steer_turn(input("original request"))
        .await?
    else {
        anyhow::bail!("initial input must start a turn");
    };
    wait_for_event(&test.codex, |event| matches!(event, EventMsg::Error(_))).await;
    let state = test.codex.state_db().expect("durable coordination owner");
    let thread: codex_protocol::ThreadId = test.session_configured.session_id.into();
    let ticket = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(ticket) = state.coordination_current_wait(thread, thread).await? {
                break anyhow::Ok(ticket);
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    assert_eq!(ticket.origin.turn_id(), turn_id);
    assert!(
        response.requests().is_empty(),
        "no inference before explicit input"
    );
    assert!(
        matches!(test.codex.steer_turn(input("explicit follow-up"), turn_id.clone()).await?,
        codex_core::SteerSubmission::Steered { turn_id: id } if id == turn_id)
    );
    wait_for_event(&test.codex, |event| {
        assert!(
            !matches!(
                event,
                EventMsg::TurnAborted(_) | EventMsg::TurnCheckpointed(_)
            ),
            "{event:?}"
        );
        matches!(event, EventMsg::TurnComplete(done) if done.turn_id == turn_id)
    })
    .await;
    let requests = response.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].body_contains_text("explicit follow-up"));
    let ticket = state
        .coordination_observe_wait(ticket.caller, ticket.id)
        .await?
        .unwrap();
    assert!(matches!(
        ticket.cancellation,
        Some(codex_state::coordination_wait::WaitCancellation::InputSuperseded { .. })
    ));
    Ok(())
}

#[test_case::test_case("insufficient_quota"; "quota")]
#[test_case::test_case("credit_balance_exhausted"; "credit_balance")]
#[test_case::test_case("organization_spend_limit_exceeded"; "organization_spend_limit")]
#[test_case::test_case("project_spend_limit_exceeded"; "project_spend_limit")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_exceeded_emits_single_error_event(code: &str) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex();

    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            json!({
                "type": "response.failed",
                "response": {
                    "id": "resp-1",
                    "error": {
                        "code": code,
                        "message": "You exceeded your current quota, please check your plan and billing details."
                    }
                }
            }),
        ]),
    )
    .await;

    let test = builder.build_with_auto_env(&server).await?;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "quota?".into(),
            text_elements: Vec::new(),
        }]))
        .await?;

    let mut error_events = 0;

    loop {
        let event = wait_for_event(&test.codex, |_| true).await;

        match event {
            EventMsg::Error(err) => {
                error_events += 1;
                assert_eq!(
                    err.message,
                    "Quota exceeded. Check your plan and billing details."
                );
                assert_eq!(
                    err.codex_error_info,
                    Some(CodexErrorInfo::UsageLimitExceeded)
                );
                let state = test.codex.state_db().expect("durable coordination owner");
                let thread: codex_protocol::ThreadId = test.session_configured.session_id.into();
                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    while state
                        .coordination_current_wait(thread, thread)
                        .await?
                        .is_none()
                    {
                        tokio::task::yield_now().await;
                    }
                    anyhow::Ok(())
                })
                .await??;
                test.codex
                    .submit(codex_protocol::protocol::Op::Interrupt)
                    .await?;
            }
            EventMsg::TurnAborted(_) => break,
            EventMsg::TurnComplete(_) | EventMsg::TurnCheckpointed(_) => {
                panic!("quota exhaustion must retain its original parked turn");
            }
            _ => {}
        }
    }

    assert_eq!(error_events, 1, "expected exactly one Codex:Error event");
    let requests = server.received_requests().await.expect("recorded requests");
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path().ends_with("/responses"))
            .count(),
        1
    );

    Ok(())
}
