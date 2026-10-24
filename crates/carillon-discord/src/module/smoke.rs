use carillon_discord_module::contract::{ActivityMarkers, CardRequest};
use carillon_protocol::model::{
    ActivityPhase, DeliveryState, Job, JobTurn, Origin, TurnActivity, TurnState,
};

/// Jobs a module must show on a card before it serves: one at work, and
/// settled ones with and without a summary to keep.
pub(super) fn cards() -> [(&'static str, CardRequest); 3] {
    let turn = |state, activity: Option<TurnActivity>, delivery| JobTurn {
        turn_id: "turn".to_owned(),
        state,
        accepted_at: 1_790_000_000,
        finished_at: (state != TurnState::Running).then_some(1_790_000_045),
        activity,
        delivery,
    };
    let request = |turn| CardRequest {
        job: Job {
            origin: Origin {
                turn_id: "turn".to_owned(),
                accepted_at: 1_790_000_000,
            },
            target_id: "1".to_owned(),
            feedback_target_id: None,
            turns: vec![turn],
            earlier: false,
        },
        seen: None,
        markers: ActivityMarkers::default(),
    };
    let answer = TurnActivity {
        phase: ActivityPhase::Model,
        headline: Some("Answering.".to_owned()),
        running: Vec::new(),
        recent: Vec::new(),
        tool_calls: 0,
        work: None,
        notes: Vec::new(),
        agent: None,
        partial: false,
        controls: Vec::new(),
    };
    [
        (
            "a job at work",
            request(turn(TurnState::Running, Some(answer.clone()), None)),
        ),
        (
            "a stopped job",
            request(turn(
                TurnState::Interrupted,
                None,
                Some(DeliveryState::Delivered),
            )),
        ),
        (
            "an answered job",
            request(turn(
                TurnState::Completed,
                Some(answer),
                Some(DeliveryState::Delivered),
            )),
        ),
    ]
}
