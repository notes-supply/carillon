//! A custom Discord module: the bundled module, with each work card coloured
//! by its job's state. Everything else is the bundled module's own handling.

use carillon_discord_module::{
    HostCalls,
    contract::{Card, Invocation, Outcome},
    handle,
};
use carillon_protocol::model::{DeliveryOutcome, Job, TurnState};
use serde_json::Value;

/// Discord's own palette.
const WORKING: u32 = 0x58_65_F2;
const ANSWERED: u32 = 0x57_F2_87;
const UNCERTAIN: u32 = 0xFE_E7_5C;
const STOPPED: u32 = 0xED_42_45;

/// The bundled module's handling, with the card it shows coloured.
pub fn coloured(host: &mut impl HostCalls, invocation: Invocation) -> Outcome {
    let colour = match &invocation {
        Invocation::Card(request) => Some(colour(&request.job)),
        Invocation::Describe | Invocation::Interaction(_) => None,
    };
    let mut outcome = handle(host, invocation);
    if let (Some(colour), Outcome::Card(Card::Show(Value::Object(embed)))) = (colour, &mut outcome)
    {
        embed.insert("color".to_owned(), colour.into());
    }
    outcome
}

fn colour(job: &Job) -> u32 {
    match job.settled() {
        None => WORKING,
        Some((_, DeliveryOutcome::Uncertain)) => UNCERTAIN,
        Some((TurnState::Completed, DeliveryOutcome::Delivered)) => ANSWERED,
        Some((
            TurnState::Interrupted | TurnState::Queued | TurnState::Running,
            DeliveryOutcome::Delivered,
        )) => STOPPED,
    }
}

carillon_discord_module::export!(coloured);

#[cfg(test)]
mod tests {
    use carillon_discord_module::{
        HostCalls,
        contract::{
            ActivityMarkers, Card, CardRequest, HostReply, HostRequest, Invocation, Outcome,
        },
    };
    use carillon_protocol::model::{DeliveryState, Job, JobTurn, Origin, TurnState};

    struct NoHost;

    impl HostCalls for NoHost {
        fn call(&mut self, _: HostRequest) -> Result<HostReply, String> {
            Err("no native calls".to_owned())
        }
    }

    fn card(state: TurnState, delivery: Option<DeliveryState>) -> Outcome {
        let request = CardRequest {
            job: Job {
                origin: Origin {
                    turn_id: "turn".to_owned(),
                    accepted_at: 1_790_000_000,
                },
                target_id: "1".to_owned(),
                turns: vec![JobTurn {
                    turn_id: "turn".to_owned(),
                    state,
                    accepted_at: 1_790_000_000,
                    finished_at: (state != TurnState::Running).then_some(1_790_000_045),
                    activity: None,
                    delivery,
                }],
                earlier: false,
            },
            seen: None,
            markers: ActivityMarkers::default(),
        };
        super::coloured(&mut NoHost, Invocation::Card(Box::new(request)))
    }

    fn colour(outcome: &Outcome) -> Option<u64> {
        match outcome {
            Outcome::Card(Card::Show(embed)) => embed["color"].as_u64(),
            _ => None,
        }
    }

    #[test]
    fn a_job_at_work_is_blurple_and_a_stopped_one_red() {
        assert_eq!(
            colour(&card(TurnState::Running, None)),
            Some(super::WORKING.into())
        );
        assert_eq!(
            colour(&card(
                TurnState::Interrupted,
                Some(DeliveryState::Delivered)
            )),
            Some(super::STOPPED.into())
        );
    }

    #[test]
    fn a_card_the_bundled_module_removes_stays_removed() {
        // An answer alone earns no summary card, coloured or not.
        assert_eq!(
            card(TurnState::Completed, Some(DeliveryState::Delivered)),
            Outcome::Card(Card::Remove)
        );
    }
}
