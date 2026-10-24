use super::*;

#[test]
fn provider_command_rejects_unsafe_identity_before_mutation() -> Result<()> {
    let directory = tempdir()?;
    let root = directory.path().join("installation");
    let config_path = initialize_test_installation(&root, directory.path(), 44_000)?;
    let before = fs::read(&config_path)?;
    crate::assert_refused(
        set_provider(&config_path, "../escape", false),
        "provider identity must be 1-128 bytes",
    )?;
    assert_eq!(fs::read(&config_path)?, before);
    Ok(())
}
