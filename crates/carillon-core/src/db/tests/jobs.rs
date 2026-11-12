use super::*;

/// Every job at work, one page after another, as `(runtime, job)`.
fn at_work(database: &Database, interface: &str) -> Result<Vec<(String, Job)>> {
    let crate::db::JobPage { mut jobs, mut next } = database
        .jobs_at_work(interface, None)?
        .context("first page")?;
    while let Some(after) = next {
        let page = database
            .jobs_at_work(interface, Some(&after))?
            .context("next page")?;
        assert!(page.jobs.len() <= JOB_READ_LIMIT);
        jobs.extend(page.jobs);
        next = page.next;
    }
    Ok(jobs)
}

#[test]
fn an_interrupted_jobs_report_is_kept_with_its_notice() -> Result<()> {
    let directory = tempdir()?;
    let database = Database::create(&directory.path().join("state.sqlite3"))?;
    accept(&database, "two", "conversation")?;
    let failed = hold(&database, "runtime")?;
    let report = report(1)?;
    database.settle_held(
        "runtime",
        &failed.turn_id,
        Settlement::Failed {
            error: "runtime failed",
            notice: "That turn was interrupted: It broke.",
            activity: Some(&report),
        },
    )?;
    let notice = database
        .claim_delivery("discord", "instance")?
        .context("notice missing")?;
    assert_eq!(notice.turn_state, TurnState::Interrupted);
    assert_eq!(notice.origin.turn_id, failed.turn_id);
    let named = database.named_jobs("discord", std::slice::from_ref(&failed.turn_id))?;
    let [(runtime, job)] = named.as_slice() else {
        bail!("expected one job: {named:?}");
    };
    assert_eq!(runtime, "runtime");
    assert_eq!(job.origin.turn_id, failed.turn_id);
    let [turn] = job.turns.as_slice() else {
        bail!("a job is one turn: {job:?}");
    };
    assert_eq!(
        (
            turn.state,
            turn.delivery,
            turn.activity.as_ref().map(|report| report.tool_calls)
        ),
        (
            TurnState::Interrupted,
            Some(DeliveryState::Delivering),
            Some(1)
        )
    );
    Ok(())
}

#[test]
fn job_reads_name_only_the_interfaces_own_jobs_and_page_through_all_at_work() -> Result<()> {
    let directory = tempdir()?;
    let database = Database::create(&directory.path().join("state.sqlite3"))?;
    // Messages of one conversation are jobs of their own.
    let one = accept(&database, "one", "conversation")?;
    let two = accept(&database, "two", "conversation")?;
    let listed = at_work(&database, "discord")?;
    assert_eq!(
        listed
            .iter()
            .map(|(_, job)| job.origin.turn_id.as_str())
            .collect::<Vec<_>>(),
        [one.turn_id.as_str(), two.turn_id.as_str()]
    );
    // Neither another interface's job nor an unknown turn names a job.
    let other = database.accept_event(
        "other",
        "runtime",
        &event("foreign", "elsewhere"),
        Admission::Queued {
            providers: &json!({}),
            resource_snapshot_id: "snapshot",
        },
        None,
    )?;
    let named = [
        other.turn_id.clone(),
        "unknown".to_owned(),
        two.turn_id.clone(),
    ];
    let listed = database.named_jobs("discord", &named)?;
    assert_eq!(
        listed
            .iter()
            .map(|(_, job)| job.origin.turn_id.as_str())
            .collect::<Vec<_>>(),
        [two.turn_id.as_str()]
    );
    assert_eq!(at_work(&database, "other")?.len(), 1);
    assert!(
        database
            .named_jobs("other", std::slice::from_ref(&one.turn_id))?
            .is_empty()
    );
    // A page cursor is one of the interface's own turns.
    assert!(
        database
            .jobs_at_work("discord", Some(&other.turn_id))?
            .is_none()
    );
    // A job that ended is no longer at work.
    let first = hold(&database, "runtime")?;
    assert_eq!(first.turn_id, one.turn_id);
    answer(&database, &one.turn_id, "done")?;
    // Every job at work is read, a bounded page at a time, oldest first.
    let mut accepted = vec![two.turn_id];
    for index in 0..2 * JOB_READ_LIMIT {
        accepted.push(
            accept(
                &database,
                &format!("many-{index}"),
                &format!("many-{index}"),
            )?
            .turn_id,
        );
    }
    let page = database
        .jobs_at_work("discord", None)?
        .context("first page")?;
    assert_eq!(page.jobs.len(), JOB_READ_LIMIT);
    assert!(page.next.is_some());
    let listed = at_work(&database, "discord")?
        .into_iter()
        .map(|(_, job)| job.origin.turn_id)
        .collect::<Vec<_>>();
    assert_eq!(listed, accepted);
    Ok(())
}
