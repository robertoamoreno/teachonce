fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).init();
    println!("recovered: {}", skillrec_recorder::recover_interrupted_sessions()?);
    Ok(())
}
