//! What a view shows an interface, whole: storage's record of its
//! conversations, and what each running turn's runtime says it is doing.

use std::collections::HashMap;

use anyhow::Result;
use carillon_protocol::{
    conversations::TurnSource,
    model::TurnActivity,
    observation::{Opened, Progress, Snapshot, View},
};

use super::Core;
use crate::db::HeldTurn;

impl Core {
    /// Everything `view` shows the authenticated interface `interface_id`.
    /// `sample` asks the runtimes what the running turns it is given are
    /// doing, and answers in their order, `None` for a turn whose runtime
    /// did not say.
    ///
    /// The order is fixed here. Which turns run is read first, their
    /// runtimes are asked outside any transaction, and one transaction then
    /// reads everything the snapshot holds. An answer is kept only for a
    /// turn that read still finds running, so a turn that ended meanwhile
    /// shows its stored ending. A turn the runtime opened offers no
    /// controls, because a control names its turn by the message that began
    /// it.
    /// # Errors
    /// Returns storage errors.
    pub(crate) async fn snapshot(
        &self,
        interface_id: &str,
        view: &View,
        sample: impl AsyncFnOnce(&[HeldTurn]) -> Vec<Option<TurnActivity>>,
    ) -> Result<Snapshot> {
        let asked = match view.open.clone() {
            Some(open) => {
                let interface = interface_id.to_owned();
                self.blocking(move |core| core.database.running_in_view(&interface, &open))
                    .await?
            }
            None => Vec::new(),
        };
        let answers = if asked.is_empty() {
            Vec::new()
        } else {
            sample(&asked).await
        };
        let (interface, shown) = (interface_id.to_owned(), view.clone());
        let mut snapshot = self
            .blocking(move |core| core.database.observed(&interface, &shown))
            .await?;
        let mut said = asked
            .into_iter()
            .zip(answers)
            .filter_map(|(held, answer)| Some((held.turn_id, answer?)))
            .collect::<HashMap<_, _>>();
        if let Some(Opened::Recorded { turns, .. }) = &mut snapshot.open {
            for turn in turns {
                match &mut turn.progress {
                    Progress::Running { activity } => {
                        *activity = said.remove(&turn.turn_id);
                        if let (TurnSource::Opened { .. }, Some(activity)) =
                            (&turn.source, activity)
                        {
                            activity.controls.clear();
                        }
                    }
                    Progress::Queued | Progress::Ended { .. } => {}
                }
            }
        }
        Ok(snapshot)
    }
}
