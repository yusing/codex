use super::*;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_protocol::ResponseItemId;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;

fn wait_output(claim: Uuid, deliveries: Vec<Uuid>) -> ResponseItemEnvelope {
    ResponseItemEnvelope {
        item: ResponseItem::FunctionCallOutput {
            id: Some(ResponseItemId::with_suffix("item", Uuid::now_v7())),
            call_id: Some("native-wait-cell".to_owned()),
            name: Some("wait_agent".to_owned()),
            namespace: None,
            output: FunctionCallOutputPayload::from_text(
                "complete frozen status and answers".to_owned(),
            ),
            internal_chat_message_metadata_passthrough: None,
        },
        metadata: Some(CodexHarnessMetadata {
            coordination_wait_claim_id: Some(claim),
            coordination_delivery_ids: deliveries,
            ..Default::default()
        }),
    }
}

#[test]
fn wait_proof_requires_complete_exact_projected_item_even_without_addressed_deliveries() {
    let claim = Uuid::now_v7();
    let output = wait_output(claim, Vec::new());
    let (omitted, ordinary) = sampling_projection(std::slice::from_ref(&output), &[]);
    assert!(omitted.wait_claim_ids.is_empty());
    assert!(ordinary.is_empty());
    let mut shortened = output.item.clone();
    if let ResponseItem::FunctionCallOutput { output, .. } = &mut shortened {
        *output = FunctionCallOutputPayload::from_text("truncated".to_owned());
    }
    assert!(
        sampling_projection(std::slice::from_ref(&output), &[shortened])
            .0
            .wait_claim_ids
            .is_empty()
    );
    let mut wrong_identity = output.item.clone();
    wrong_identity.set_id(Some(ResponseItemId::with_suffix("item", Uuid::now_v7())));
    assert!(
        sampling_projection(std::slice::from_ref(&output), &[wrong_identity])
            .0
            .wait_claim_ids
            .is_empty()
    );
    let (valid, ordinary) = sampling_projection(
        std::slice::from_ref(&output),
        std::slice::from_ref(&output.item),
    );
    assert_eq!(valid.wait_claim_ids, HashSet::from([claim]));
    assert!(valid.delivery_ids.is_empty());
    assert!(ordinary.is_empty());
}

#[test]
fn wait_owned_ids_are_not_generically_consumed_and_shortening_revokes_proof() {
    let claim = Uuid::now_v7();
    let delivery = Uuid::now_v7();
    let mut output = wait_output(claim, vec![delivery]);
    let (evidence, ordinary) = sampling_projection(
        std::slice::from_ref(&output),
        std::slice::from_ref(&output.item),
    );
    assert_eq!(evidence.delivery_ids, HashSet::from([delivery]));
    assert!(ordinary.is_empty());
    output
        .metadata
        .as_mut()
        .unwrap()
        .mark_retained_sources_incomplete();
    let (evidence, ordinary) = sampling_projection(
        std::slice::from_ref(&output),
        std::slice::from_ref(&output.item),
    );
    assert_eq!(evidence, CoordinationSamplingEvidence::default());
    assert!(ordinary.is_empty());
}

#[test]
fn ordinary_human_input_keeps_role_and_authenticated_ids_survive_retention() {
    let id = Uuid::now_v7();
    let content = vec![UserInput::Text {
        text: "human child input".to_owned(),
        text_elements: Vec::new(),
    }];
    let payload = CoordinationPayload::UserInput {
        content: content.clone(),
    };
    let record = super::super::work::DeliveryRecord {
        sequence: 1,
        publication: PublishDelivery {
            id,
            root_id: ThreadId::new(),
            sender_id: None,
            recipient_id: ThreadId::new(),
            work: None,
            kind: DeliveryKind::Message,
            payload: serde_json::to_string(&payload).unwrap(),
        },
        claim_id: None,
        dispatch_id: Some(id),
        accepted_receipt: Some("native-input".to_owned()),
        refused_reason: None,
        consumed: false,
    };
    let input = delivery_input(&record, None, AgentPath::root()).unwrap();
    let TurnInput::UserInput {
        content: actual,
        metadata,
        ..
    } = &input
    else {
        panic!("human input was converted to agent mail");
    };
    assert_eq!(actual, &content);
    assert_eq!(metadata.coordination_delivery_ids, vec![id]);
    let restored: TurnInput =
        serde_json::from_value(serde_json::to_value(&input).unwrap()).unwrap();
    assert_eq!(input_delivery_ids(&restored), &[id]);
}

#[test]
fn protocol_agent_input_cannot_supply_native_delivery_proof() {
    let id = Uuid::now_v7();
    let message = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::root(),
        Vec::new(),
        "message".to_owned(),
        false,
    );
    let unadmitted = crate::PendingAgentMessage::from(message);
    assert!(unadmitted.coordination_delivery_ids().is_empty());
    let admitted = unadmitted.with_coordination_delivery_ids(vec![id]);
    let restored: crate::PendingAgentMessage =
        serde_json::from_value(serde_json::to_value(&admitted).unwrap()).unwrap();
    assert_eq!(restored.coordination_delivery_ids(), &[id]);
}

#[tokio::test]
async fn private_or_unregistered_sessions_have_no_coordination_wake_or_drain() {
    let (session, _turn) = crate::session::tests::make_session_and_context().await;
    assert!(!session.has_pending_coordination_messages().await.unwrap());
    assert!(
        session
            .take_coordination_messages()
            .await
            .unwrap()
            .is_empty()
    );
}
