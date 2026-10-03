//! Prices: the table the usage figures use, and `harness pricing update`, the only time harness
//! connects to models.dev.

use harness_usage::{
    paths::Dirs,
    pricing::{self, MODELS_DEV_URL},
};

use crate::term::terminal_safe;

/// `harness pricing update`: fetches the table, stores it, and says how many models it prices.
/// Exit code 1, with the old table left in place, when the fetch or the data is no good.
pub async fn update() -> u8 {
    let setup = match crate::setup::load() {
        Ok(setup) => setup,
        Err(message) => {
            eprintln!("error: {}", terminal_safe(&message));
            return 2;
        }
    };
    // For tests, in debug builds only: another address than models.dev's.
    let url = harness_providers::registry::test_hook("HARNESS_PRICING_URL", |var| (setup.env)(var))
        .unwrap_or_else(|| MODELS_DEV_URL.to_string());
    let dest = Dirs::under(&setup.paths.data_dir).pricing;
    let today = harness_core::time::today_utc();
    match pricing::update(&url, &dest, &today).await {
        Ok(updated) => {
            println!(
                "Updated the price table: {} models, dated {} (stored in {}).",
                updated.models,
                updated.date,
                terminal_safe(&updated.path.display().to_string())
            );
            0
        }
        Err(e) => {
            eprintln!(
                "error: {} The previous table stays in use.",
                terminal_safe(&e.to_string())
            );
            1
        }
    }
}
