//! What the session knows of usage that the UI cannot find out by itself.

/// Settings of the usage reports, given by the host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageContext {
    /// The `<provider>/<model>` the avoided figure is measured against (`usage.baseline`).
    pub baseline: Option<String>,
    /// The price table in use and its date, such as `embedded 2026-10-03`.
    pub prices: String,
}
