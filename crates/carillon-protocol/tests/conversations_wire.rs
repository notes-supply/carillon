//! The wire shape of an interface's history reads, pinned by examples that
//! round-trip unchanged.
use anyhow::Result;
use carillon_protocol::conversations::{ConversationPage, Transcript};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct Fixtures {
    pages: Vec<Value>,
    transcripts: Vec<Value>,
}

#[test]
fn history_reads_keep_their_wire_shape() -> Result<()> {
    let fixtures: Fixtures = serde_json::from_str(include_str!("fixtures/conversations.json"))?;
    for page in &fixtures.pages {
        let parsed: ConversationPage = serde_json::from_value(page.clone())?;
        assert_eq!(serde_json::to_value(parsed)?, *page);
    }
    for transcript in &fixtures.transcripts {
        let parsed: Transcript = serde_json::from_value(transcript.clone())?;
        assert_eq!(serde_json::to_value(parsed)?, *transcript);
    }
    Ok(())
}
